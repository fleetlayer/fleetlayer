//! Rex, in Rex's own terms: its server API called through the public `rex` command line (`rex api call <method>
//! <json>`, `rex events --all --json`, argv only, never a shell). Every method and field used here is in Rex's public
//! docs (https://www.superlogical.com/rex/docs/reference/api) and in what `rex api describe` prints.
//!
//! Rex's model: a server holds sessions, a session holds windows, a window holds blocks, and a terminal is one kind
//! of block. Fleetlayer's session is one agent running in one terminal block.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fleetlayer_protocol::ext::Status;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

/// The creator of Rex's terminal blocks.
pub const TERMINAL: &str = "com.superlogical.terminal";

/// Agent programs this plugin recognises in a terminal, by the program's name (or the `app` it reports with OSC
/// 7501). Anything else in a terminal (a shell, an editor) isn't a session.
const HARNESSES: &[&str] =
    &["claude", "codex", "opencode", "pi", "gemini", "crush", "amp", "cursor-agent", "goose", "aider", "droid"];

/// How to run `rex`: the program, and the server it talks to (`-S`) when it isn't this user's own.
#[derive(Debug, Clone)]
pub struct Rex {
    pub program: String,
    pub server: Option<String>,
}

/// What a call can fail with.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The session or block isn't there.
    NotHere(String),
    /// Anything else, as a sentence.
    Other(String),
}

impl Failure {
    pub fn message(&self) -> &str {
        match self {
            Failure::NotHere(m) | Failure::Other(m) => m,
        }
    }
}

impl Rex {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.program);
        if let Some(s) = &self.server {
            c.args(["-S", s]);
        }
        // Never start a server of its own: the person's server is the one to read, and a missing one is an error.
        c.arg("--autostart=false");
        c.kill_on_drop(true);
        c
    }

    /// `rex api call <method> <params>`, with Rex's error as a sentence.
    pub async fn call(&self, method: &str, params: &Value) -> Result<Value, Failure> {
        let run = self.command().args(["api", "call", method, &params.to_string()]).output();
        let out = match tokio::time::timeout(Duration::from_secs(15), run).await {
            Ok(r) => r.map_err(|e| Failure::Other(spawn_error(&self.program, &e)))?,
            Err(_) => return Err(Failure::Other(format!("rex api call {method} didn't answer within 15 seconds"))),
        };
        if !out.status.success() {
            return Err(classify(&String::from_utf8_lossy(&out.stderr)));
        }
        if out.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(json!({}));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| Failure::Other(format!("reading rex api call {method}: {e}")))
    }

    /// A block method (`block.method`), such as a terminal's `process` or `write`.
    pub async fn block(&self, session: &str, block: &str, method: &str, args: Value) -> Result<Value, Failure> {
        self.call("block.method", &json!({"session_id": session, "block_id": block, "method": method, "args": args}))
            .await
    }

    /// Every agent running in a terminal on the server, with its status.
    pub async fn snapshot(&self, claude_sessions: &Path) -> Result<Vec<Agent>, Failure> {
        let list: SessionList = from(self.call("session.list", &json!({})).await?)?;
        let registrations = claude_registrations(claude_sessions).await;
        let mut agents = vec![];
        for s in list.sessions {
            let view: View = match self.call("session.view", &json!({"session_id": s.session_id})).await {
                Ok(v) => from(v)?,
                // A session that closed while it was being read.
                Err(Failure::NotHere(_)) => continue,
                Err(e) => return Err(e),
            };
            let terminals: Vec<&ViewBlock> = view.blocks().filter(|b| b.creator_name == TERMINAL).collect();
            let mut found = vec![];
            for b in &terminals {
                let Ok(process) = self.block(&s.session_id, &b.block_id, "process", json!({})).await else { continue };
                let process: Process = serde_json::from_value(process).unwrap_or_default();
                let records = match self.block(&s.session_id, &b.block_id, "program_status", json!({})).await {
                    Ok(v) => serde_json::from_value::<StatusRecords>(v).unwrap_or_default().records,
                    Err(_) => vec![],
                };
                let Some(harness) = harness(&process, &records) else { continue };
                let session_id = claude_session(&harness, &process, &registrations);
                found.push(Agent {
                    session: s.session_id.clone(),
                    session_label: s.label.clone(),
                    block: b.block_id.clone(),
                    block_label: b.label.clone(),
                    harness,
                    session_id,
                    cwd: process.foreground.as_ref().and_then(|p| p.cwd.clone()),
                    status: status(&records),
                    tab: None,
                    human_attached: None,
                });
            }
            if found.is_empty() {
                continue;
            }
            let human = match self.call("session.list_clients", &json!({"session_id": s.session_id})).await {
                Ok(v) => Some(human_attached(&v)),
                Err(_) => None,
            };
            let several = found.len() > 1;
            for mut a in found {
                a.tab =
                    Some(tab_name(a.session_label.as_deref(), &a.session, a.block_label.as_deref(), &a.block, several));
                a.human_attached = human;
                agents.push(a);
            }
        }
        Ok(agents)
    }

    /// A terminal's status now, from the program status records in it.
    pub async fn status_of(&self, session: &str, block: &str) -> Result<Status, Failure> {
        let v = self.block(session, block, "program_status", json!({})).await?;
        Ok(status(&serde_json::from_value::<StatusRecords>(v).unwrap_or_default().records))
    }

    /// The Claude Code session ID of the agent in a terminal, once Claude Code has registered it.
    pub async fn claude_session_of(&self, session: &str, block: &str, claude_sessions: &Path) -> Option<String> {
        let process: Process =
            serde_json::from_value(self.block(session, block, "process", json!({})).await.ok()?).unwrap_or_default();
        claude_session("claude", &process, &claude_registrations(claude_sessions).await)
    }

    /// `rex new <label> --cwd <cwd> --json -- <program> <args...>`: a new session running the agent. Rex starts it
    /// through the user's login shell, so the agent finds what's on the person's PATH. Answers the session and block.
    pub async fn new_session(
        &self,
        label: &str,
        cwd: &str,
        program: &str,
        args: &[String],
    ) -> Result<(String, String), Failure> {
        let mut c = self.command();
        c.args(new_args(label, cwd, program, args));
        let out = match tokio::time::timeout(Duration::from_secs(30), c.output()).await {
            Ok(r) => r.map_err(|e| Failure::Other(spawn_error(&self.program, &e)))?,
            Err(_) => return Err(Failure::Other("rex new didn't answer within 30 seconds".into())),
        };
        if !out.status.success() {
            return Err(classify(&String::from_utf8_lossy(&out.stderr)));
        }
        let created: Created =
            serde_json::from_slice(&out.stdout).map_err(|e| Failure::Other(format!("reading rex new: {e}")))?;
        let block = created
            .initial_windows
            .iter()
            .flat_map(|w| w.block_ids.iter())
            .next()
            .cloned()
            .ok_or_else(|| Failure::Other("rex new named no terminal".into()))?;
        Ok((created.session_id, block))
    }

    /// Type a message into an agent and submit it: a paste (so newlines stay in one message), then Return.
    pub async fn type_message(&self, session: &str, block: &str, text: &str) -> Result<(), Failure> {
        self.call(
            "block.event",
            &json!({"session_id": session, "block_id": block, "event": {"type": "paste", "payload": {"text": text}}}),
        )
        .await?;
        // The agent takes the paste in before the Return, or the Return lands inside the pasted text.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.block(session, block, "write", json!({"data": "\r"})).await.map(|_| ())
    }

    /// Press Esc in a terminal: an agent stops its turn.
    pub async fn interrupt(&self, session: &str, block: &str) -> Result<(), Failure> {
        self.block(session, block, "write", json!({"data": "\u{1b}"})).await.map(|_| ())
    }

    /// Close the terminal block the agent runs in (and its session, if it was the last block).
    /// Rex keeps a session whose last block closed, so an agent alone in its session closes the session, as closing
    /// a tab would.
    pub async fn close(&self, session: &str, block: &str) -> Result<(), Failure> {
        let blocks = self.call("session.list_blocks", &json!({"session_id": session})).await?;
        let others = blocks
            .get("blocks")
            .and_then(Value::as_array)
            .is_some_and(|bs| bs.iter().any(|b| b.get("block_id").and_then(Value::as_str) != Some(block)));
        if others {
            self.call("block.close", &json!({"session_id": session, "block_id": block})).await.map(|_| ())
        } else {
            self.call("session.destroy", &json!({"session_id": session})).await.map(|_| ())
        }
    }

    /// `rex events --all --json`: every server event, one JSON object per line, for as long as the server runs.
    pub fn events(&self) -> Result<Events, Failure> {
        let mut c = self.command();
        c.args(["events", "--all", "--json"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let mut child = c.spawn().map_err(|e| Failure::Other(spawn_error(&self.program, &e)))?;
        let stdout = child.stdout.take().ok_or_else(|| Failure::Other("rex events has no output".into()))?;
        Ok(Events { _child: child, lines: BufReader::new(stdout).lines() })
    }
}

pub fn new_args(label: &str, cwd: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["new", label, "--cwd", cwd, "--json", "--", program].map(String::from).into();
    a.extend(args.iter().cloned());
    a
}

fn from<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Failure> {
    serde_json::from_value(v).map_err(|e| Failure::Other(format!("reading Rex's answer: {e}")))
}

/// Why `rex` couldn't be run, as a sentence: a missing program says how to point the plugin at it.
pub fn spawn_error(rex: &str, e: &std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("running {rex}: {e}");
    }
    let fix = "give the rex plugin `config: { program: <full path to rex> }` in node.yaml";
    if rex.contains('/') {
        format!("there's no rex at {rex}: {fix}")
    } else {
        let path = std::env::var("PATH").unwrap_or_default();
        format!("{rex} isn't on the node's PATH ({path}): {fix}")
    }
}

/// Rex's error (`Error: … (status=404 code=session_not_found)`) as a sentence, sorted into a kind.
pub fn classify(stderr: &str) -> Failure {
    let line = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("rex failed");
    let message = line.strip_prefix("Error: ").unwrap_or(line).to_string();
    let code = message.rsplit_once("code=").map(|(_, c)| c.trim_end_matches(')').to_string());
    match code.as_deref() {
        Some("session_not_found" | "not_found") => Failure::NotHere(message),
        _ => Failure::Other(message),
    }
}

// ---------------------------------------------------------------- what Rex answers

#[derive(Debug, Deserialize)]
struct SessionList {
    #[serde(default)]
    sessions: Vec<SessionRef>,
}

#[derive(Debug, Deserialize)]
struct SessionRef {
    session_id: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct View {
    #[serde(default)]
    windows: Vec<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    #[serde(default)]
    layers: Vec<Layer>,
}

#[derive(Debug, Deserialize)]
struct Layer {
    #[serde(default)]
    blocks: Vec<ViewBlock>,
}

#[derive(Debug, Deserialize)]
struct ViewBlock {
    block_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    creator_name: String,
}

impl View {
    fn blocks(&self) -> impl Iterator<Item = &ViewBlock> {
        self.windows.iter().flat_map(|w| w.layers.iter()).flat_map(|l| l.blocks.iter())
    }
}

#[derive(Debug, Deserialize)]
struct Created {
    session_id: String,
    #[serde(default)]
    initial_windows: Vec<CreatedWindow>,
}

#[derive(Debug, Deserialize)]
struct CreatedWindow {
    #[serde(default)]
    block_ids: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Process {
    #[serde(default)]
    pub child: Option<ProcessInfo>,
    #[serde(default)]
    pub foreground: Option<ProcessInfo>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ProcessInfo {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ProcessInfo {
    /// The program's name as Rex says to show it: the first of script, argv0 and name, without its folder.
    fn program(&self) -> Option<String> {
        let p = self.script.as_deref().or(self.argv0.as_deref()).or(self.name.as_deref())?;
        Some(Path::new(p).file_name()?.to_string_lossy().into_owned())
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct StatusRecords {
    #[serde(default)]
    pub records: Vec<StatusRecord>,
}

/// One program status record (OSC 7501).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct StatusRecord {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub effective_app: Option<String>,
}

/// One agent in a Rex terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub session: String,
    pub session_label: Option<String>,
    pub block: String,
    pub block_label: Option<String>,
    pub harness: String,
    /// The harness's own session ID, when it's known (Claude Code's, from its session registry).
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub status: Status,
    pub tab: Option<String>,
    pub human_attached: Option<bool>,
}

fn normalise(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "claude-code" | "claude code" => "claude".into(),
        _ => n,
    }
}

/// The agent a terminal runs: its foreground program when that's an agent, else the app its status records name.
pub fn harness(process: &Process, records: &[StatusRecord]) -> Option<String> {
    let known = |n: String| HARNESSES.contains(&n.as_str()).then_some(n);
    if let Some(h) = process.foreground.as_ref().and_then(ProcessInfo::program).map(|p| normalise(&p)).and_then(known) {
        return Some(h);
    }
    records
        .iter()
        .find(|r| r.id.as_deref().unwrap_or("").is_empty())
        .and_then(|r| r.effective_app.as_deref().or(r.app.as_deref()))
        .map(normalise)
        .and_then(known)
}

/// A terminal's status from its records: blocked anywhere wins (a person is needed), then the root record's state.
/// `error` is a finished turn that failed, so it reads as `done`; a state Rex adds later reads as unknown.
pub fn status(records: &[StatusRecord]) -> Status {
    if records.iter().any(|r| r.state == "blocked") {
        return Status::Blocked;
    }
    let root = records.iter().find(|r| r.id.as_deref().unwrap_or("").is_empty());
    let state =
        root.map(|r| r.state.as_str()).or_else(|| records.iter().any(|r| r.state == "working").then_some("working"));
    match state {
        Some("idle") => Status::Idle,
        Some("working") => Status::Working,
        Some("done" | "error") => Status::Done,
        _ => Status::Unknown,
    }
}

/// Whether a person has the session open in a Rex app (a Mac, phone or Windows client), not only the command line.
pub fn human_attached(clients: &Value) -> bool {
    clients
        .get("clients")
        .and_then(Value::as_array)
        .is_some_and(|cs| cs.iter().any(|c| c.pointer("/info/kind").and_then(Value::as_str) == Some("app")))
}

/// The session's name in the fleet: the Rex session's label, or `<session>/<block>` when the session runs several
/// agents. A session or block with no label goes by the short form of its ID.
pub fn tab_name(
    session_label: Option<&str>,
    session: &str,
    block_label: Option<&str>,
    block: &str,
    several: bool,
) -> String {
    let short = |id: &str| {
        let uuid = id.split_once(':').map_or(id, |(_, u)| u);
        uuid[uuid.len().saturating_sub(8)..].to_string()
    };
    let s = session_label.filter(|l| !l.is_empty()).map_or_else(|| short(session), str::to_string);
    if !several {
        return s;
    }
    let b = block_label.filter(|l| !l.is_empty() && *l != "shell").map_or_else(|| short(block), str::to_string);
    format!("{s}/{b}")
}

// ---------------------------------------------------------------- Claude Code's session registry

/// Claude Code registers every interactive session in `~/.claude/sessions/<pid>.json`, with its process and its
/// session ID. That's how a terminal's Claude Code gets the ID the fleet knows it by.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub pid: u32,
    pub session_id: String,
}

pub async fn claude_registrations(dir: &Path) -> Vec<Registration> {
    let mut out = vec![];
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Ok(bytes) = tokio::fs::read(&p).await
            && let Ok(r) = serde_json::from_slice::<Registration>(&bytes)
        {
            out.push(r);
        }
    }
    out
}

pub fn claude_session(harness: &str, process: &Process, regs: &[Registration]) -> Option<String> {
    if harness != "claude" {
        return None;
    }
    let pids: Vec<u32> = [&process.foreground, &process.child].into_iter().flatten().filter_map(|p| p.pid).collect();
    regs.iter().find(|r| pids.contains(&r.pid)).map(|r| r.session_id.clone())
}

/// Claude Code's directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"))
}

// ---------------------------------------------------------------- events

/// One server event, in the plugin's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A terminal's program status changed.
    Status {
        session: String,
        block: String,
    },
    /// The program in a terminal changed: an agent may have started or exited.
    Process {
        session: String,
        block: String,
    },
    BlockClosed {
        session: String,
        block: String,
    },
    SessionDestroyed {
        session: String,
    },
    /// Something `snapshot` reads differently now (a new session, a new layout, a new label).
    Changed,
    Other,
}

pub fn parse_event(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Event::Other };
    let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
    let p = v.get("payload").cloned().unwrap_or(Value::Null);
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match kind {
        "session_created" => Event::Changed,
        "session_destroyed" => match s(&p, "session_id") {
            Some(session) => Event::SessionDestroyed { session },
            None => Event::Changed,
        },
        "session_event" => {
            let Some(session) = s(&p, "session_id") else { return Event::Other };
            let e = p.get("event").cloned().unwrap_or(Value::Null);
            let ep = e.get("payload").cloned().unwrap_or(Value::Null);
            match e.get("type").and_then(Value::as_str).unwrap_or_default() {
                "block_event" => {
                    let Some(block) = s(&ep, "block_id") else { return Event::Other };
                    match ep.get("name").and_then(Value::as_str) {
                        Some("program_status_changed") => Event::Status { session, block },
                        Some("process_changed") => Event::Process { session, block },
                        _ => Event::Other,
                    }
                }
                "block_closed" => match s(&ep, "block_id") {
                    Some(block) => Event::BlockClosed { session, block },
                    None => Event::Changed,
                },
                "session_view_changed" | "session_label_changed" | "block_label_changed" => Event::Changed,
                t if t.ends_with("_label_changed") || t.ends_with("_renamed") => Event::Changed,
                _ => Event::Other,
            }
        }
        _ => Event::Other,
    }
}

/// A running `rex events` stream.
pub struct Events {
    _child: tokio::process::Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Events {
    /// The next event; None when the stream ended (the server stopped or restarted).
    pub async fn next(&mut self) -> std::io::Result<Option<Event>> {
        Ok(self.lines.next_line().await?.map(|l| parse_event(&l)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: Option<&str>, state: &str, app: Option<&str>) -> StatusRecord {
        StatusRecord {
            id: id.map(str::to_string),
            state: state.into(),
            app: app.map(str::to_string),
            effective_app: app.map(str::to_string),
        }
    }

    fn fg(name: &str, pid: u32) -> Process {
        Process {
            child: None,
            foreground: Some(ProcessInfo { pid: Some(pid), name: Some(name.into()), ..Default::default() }),
        }
    }

    #[test]
    fn a_missing_rex_says_how_to_point_at_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(
            spawn_error("rex", &gone)
                .ends_with("give the rex plugin `config: { program: <full path to rex> }` in node.yaml")
        );
        assert!(spawn_error("/opt/rex", &gone).starts_with("there's no rex at /opt/rex: "));
    }

    // As rex 0.1.0-preview.25 printed them (27 November 2026).
    #[test]
    fn rex_errors_read_as_sentences_and_sort_into_kinds() {
        let missing =
            "Error: stream response \"control-2\" failed: session not found (status=404 code=session_not_found)\n";
        assert!(matches!(classify(missing), Failure::NotHere(m) if m.starts_with("stream response")));
        let method =
            "Error: stream response \"control-2\" failed: method not found: \"x\" (status=404 code=method_not_found)";
        assert!(matches!(classify(method), Failure::Other(_)));
        assert_eq!(classify(""), Failure::Other("rex failed".into()));
    }

    #[test]
    fn status_comes_from_the_program_status_records() {
        assert_eq!(status(&[]), Status::Unknown);
        assert_eq!(status(&[rec(None, "idle", Some("claude"))]), Status::Idle);
        assert_eq!(status(&[rec(Some(""), "working", None)]), Status::Working);
        assert_eq!(status(&[rec(None, "done", None)]), Status::Done);
        assert_eq!(status(&[rec(None, "error", None)]), Status::Done, "a failed turn is finished, waiting");
        assert_eq!(
            status(&[rec(None, "idle", None), rec(Some("build/test"), "blocked", None)]),
            Status::Blocked,
            "a blocked child needs a person even when the root is idle"
        );
        assert_eq!(status(&[rec(Some("worker"), "working", None)]), Status::Working);
        assert_eq!(status(&[rec(None, "napping", None)]), Status::Unknown);
    }

    #[test]
    fn an_agent_is_its_foreground_program_or_the_app_it_reports() {
        assert_eq!(harness(&fg("claude", 1), &[]).as_deref(), Some("claude"));
        assert_eq!(harness(&fg("zsh", 1), &[]), None, "a shell isn't a session");
        assert_eq!(harness(&fg("node", 1), &[rec(None, "idle", Some("claude"))]).as_deref(), Some("claude"));
        assert_eq!(harness(&fg("node", 1), &[rec(None, "idle", Some("terraform"))]), None);
        let wrapped = Process {
            child: None,
            foreground: Some(ProcessInfo {
                argv0: Some("/usr/local/bin/codex".into()),
                name: Some("node".into()),
                ..Default::default()
            }),
        };
        assert_eq!(harness(&wrapped, &[]).as_deref(), Some("codex"));
    }

    #[test]
    fn claude_code_sessions_are_found_by_their_process() {
        let regs =
            vec![Registration { pid: 7, session_id: "s7".into() }, Registration { pid: 9, session_id: "s9".into() }];
        assert_eq!(claude_session("claude", &fg("claude", 9), &regs).as_deref(), Some("s9"));
        assert_eq!(claude_session("claude", &fg("claude", 8), &regs), None);
        assert_eq!(claude_session("codex", &fg("codex", 9), &regs), None);
    }

    #[test]
    fn tab_names_follow_rex_labels() {
        let s = "session:01a21c29-0516-7e54-9d15-1e73428d0277";
        let b = "block:01a21c29-0518-7e1b-a413-6df5eceef2f0";
        assert_eq!(tab_name(Some("planning"), s, Some("shell"), b, false), "planning");
        assert_eq!(tab_name(None, s, None, b, false), "428d0277", "no label: the short ID");
        assert_eq!(tab_name(Some("planning"), s, Some("review"), b, true), "planning/review");
        assert_eq!(tab_name(Some("planning"), s, Some("shell"), b, true), "planning/eceef2f0");
    }

    #[test]
    fn a_person_is_attached_when_an_app_is() {
        assert!(human_attached(
            &json!({"clients": [{"info": {"kind": "cli"}}, {"info": {"kind": "app", "platform": "ios"}}]})
        ));
        assert!(!human_attached(&json!({"clients": [{"info": {"kind": "cli"}}]})));
        assert!(!human_attached(&json!({"clients": null})));
    }

    #[test]
    fn new_sessions_run_the_agent_after_the_separator() {
        assert_eq!(
            new_args("esign cert", "/w", "claude", &["--name".into(), "esign cert".into()]),
            ["new", "esign cert", "--cwd", "/w", "--json", "--", "claude", "--name", "esign cert"]
        );
    }

    // Lines as `rex events --all --json` printed them (rex 0.1.0-preview.25, 27 November 2026), shortened.
    #[test]
    fn rex_events_parse() {
        let status = r#"{"type":"session_event","payload":{"session_id":"session:a","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"program_status_changed","payload":{"id":"","record":{"state":"idle","app":"claude","effective_app":"claude","updated_at":"2026-11-27T21:40:09.585211965+13:00","owner":{"pid":411469,"name":"dash"}},"reason":"report"}}}}}"#;
        assert_eq!(parse_event(status), Event::Status { session: "session:a".into(), block: "block:b".into() });
        let process = r#"{"type":"session_event","payload":{"session_id":"session:a","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"process_changed","payload":{"generation":2,"foreground":{"pid":413174,"user_id":1001,"user":"nathan","name":"dash","path":"/usr/bin/dash","argv0":"/bin/sh","cwd":"/tmp"}}}}}}"#;
        assert_eq!(parse_event(process), Event::Process { session: "session:a".into(), block: "block:b".into() });
        let closed = r#"{"type":"session_event","payload":{"session_id":"session:a","event":{"type":"block_closed","payload":{"block_id":"block:b"}}}}"#;
        assert_eq!(parse_event(closed), Event::BlockClosed { session: "session:a".into(), block: "block:b".into() });
        let gone = r#"{"type":"session_destroyed","payload":{"session_id":"session:a"}}"#;
        assert_eq!(parse_event(gone), Event::SessionDestroyed { session: "session:a".into() });
        let created = r#"{"type":"session_created","payload":{"session":{"session_id":"session:c","label":"second"}}}"#;
        assert_eq!(parse_event(created), Event::Changed);
        let view = r#"{"type":"session_event","payload":{"session_id":"session:a","event":{"type":"session_view_changed","payload":{"revision":2}}}}"#;
        assert_eq!(parse_event(view), Event::Changed);
        let joined = r#"{"type":"client_joined","payload":{"client":{"client_id":"client:x"}}}"#;
        assert_eq!(parse_event(joined), Event::Other);
        assert_eq!(parse_event("not json"), Event::Other);
    }
}
