//! The harnesses Fleetlayer follows through their own hooks (Gemini CLI, Cursor's CLI), as the Claude Code plugin does.
//!
//! The plugin adds a few hooks to the harness's settings (every other key and hook kept, the file replaced in one
//! rename). Each runs this binary, `fleetlayer-harness hook <harness>`, by a path that doesn't depend on the agent's
//! PATH; it adds the agent's process ID and pane to the event, passes it to the plugin's socket, and exits 0 quickly
//! whatever happens, so it never holds the agent up or changes what it does.
//!
//! In a herdr pane, Gemini CLI's hook also reports the session's state to herdr ([`report_to_herdr`]), which doesn't
//! recognise Gemini CLI's process everywhere and then wouldn't list the pane at all.
//!
//! From the events the [`Tracker`] keeps each session: its folder, transcript, process, pane and the permission
//! prompt it shows. Neither harness can take a message into a running session from outside, so the node types
//! messages in (these sessions are listed with `deliverable: false`).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fleetlayer_protocol::ext::{BlockedPrompt, PromptKind, PromptTool};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::Kind;

/// What marks a hook as this plugin's, so installing again replaces it and nothing else.
const MARK: &str = "fleetlayer-harness";

/// The hook's command: this binary by a path that doesn't depend on the agent's PATH, quiet and successful whatever
/// happens. Under the home folder it's written from `$HOME`, so a settings file shared between machines works on each.
pub fn command(exe: &Path, home: &Path, kind: Kind) -> String {
    let quote = |p: &str| p.replace('\'', "'\\''");
    let h = kind.name();
    match exe.strip_prefix(home) {
        Ok(rel) if !home.as_os_str().is_empty() => {
            format!("\"$HOME\"/'{}' hook {h} 2>/dev/null || true", quote(&rel.to_string_lossy()))
        }
        _ => format!("'{}' hook {h} 2>/dev/null || true", quote(&exe.to_string_lossy())),
    }
}

pub fn socket_path(state: &Path) -> PathBuf {
    state.join("hooks.sock")
}

const GEMINI_EVENTS: &[&str] =
    &["SessionStart", "SessionEnd", "BeforeAgent", "AfterAgent", "Notification", "AfterTool"];
const CURSOR_EVENTS: &[&str] = &["sessionStart", "sessionEnd", "beforeSubmitPrompt", "stop"];

fn ours(command: Option<&str>) -> bool {
    command.is_some_and(|c| c.contains(MARK) && c.contains(" hook "))
}

/// Put the hooks, running `command`, in Gemini CLI's `settings.json` (`want: None` takes them out). True when the file
/// changed.
pub fn edit_gemini(settings: &Path, want: Option<&str>) -> Result<bool, String> {
    if want.is_none() && !settings.exists() {
        return Ok(false);
    }
    crate::files::edit_json(settings, |root| {
        let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
        let hooks = hooks.as_object_mut().ok_or("hooks in Gemini CLI's settings isn't an object")?;
        for event in GEMINI_EVENTS {
            let list = hooks.entry(*event).or_insert_with(|| Value::Array(vec![]));
            let list = list.as_array_mut().ok_or_else(|| format!("hooks.{event} isn't a list"))?;
            list.retain(|e| !e["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| ours(h["command"].as_str()))));
            if let Some(c) = want {
                list.push(json!({"hooks": [{"type": "command", "name": "fleetlayer", "command": c}]}));
            }
        }
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        if root.get("hooks").and_then(Value::as_object).is_some_and(Map::is_empty) {
            root.remove("hooks");
        }
        Ok(())
    })
}

/// Put the hooks in Cursor's `hooks.json` (`{"version":1,"hooks":{"<event>":[{"command"}]}}`).
pub fn edit_cursor(file: &Path, want: Option<&str>) -> Result<bool, String> {
    if want.is_none() && !file.exists() {
        return Ok(false);
    }
    crate::files::edit_json(file, |root| {
        root.entry("version").or_insert(json!(1));
        let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
        let hooks = hooks.as_object_mut().ok_or("hooks in Cursor's hooks.json isn't an object")?;
        for event in CURSOR_EVENTS {
            let list = hooks.entry(*event).or_insert_with(|| Value::Array(vec![]));
            let list = list.as_array_mut().ok_or_else(|| format!("hooks.{event} isn't a list"))?;
            list.retain(|h| !ours(h["command"].as_str()));
            if let Some(c) = want {
                list.push(json!({"command": c}));
            }
        }
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        Ok(())
    })
}

/// `fleetlayer-harness hook <harness>`: pass the event on stdin to the plugin, with the agent's process and pane added,
/// never failing. Cursor reads a hook's answer from its output: an empty object, and `continue` for a prompt.
pub fn run_hook(kind: Kind) {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut input);
    if kind == Kind::Cursor {
        let event = serde_json::from_slice::<Value>(&input).ok();
        let submit = event.as_ref().is_some_and(|v| v["hook_event_name"] == "beforeSubmitPrompt");
        println!("{}", if submit { r#"{"continue":true}"# } else { "{}" });
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(&input) else { return };
    if kind == Kind::Gemini {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        report_to_herdr(&v, &crate::files::state_dir(&home, kind.name()));
    }
    let Some(obj) = v.as_object_mut() else { return };
    obj.insert(
        "_fleetlayer".into(),
        json!({"pid": crate::files::agent_pid(), "address": std::env::var("HERDR_PANE_ID").ok().filter(|a| !a.is_empty())}),
    );
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let path = socket_path(&crate::files::state_dir(&home, kind.name()));
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(path) {
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        let mut line = v.to_string();
        line.push('\n');
        let _ = s.write_all(line.as_bytes());
    }
}

/// The state a Gemini CLI hook event means for herdr: working, idle, blocked, or released (`None`) when it ends.
fn herdr_state(v: &Value) -> Option<Option<&'static str>> {
    match (v["hook_event_name"].as_str()?, v["notification_type"].as_str()) {
        ("SessionStart" | "AfterAgent", _) => Some(Some("idle")),
        ("BeforeAgent" | "AfterTool", _) => Some(Some("working")),
        ("Notification", Some("ToolPermission")) => Some(Some("blocked")),
        ("SessionEnd", _) => Some(None),
        _ => None,
    }
}

/// Tell herdr, through its documented reporting API, that this pane runs Gemini CLI and what it's doing. herdr reads
/// Gemini CLI's state off the screen and doesn't recognise its process everywhere (it runs as `node`), and a pane it
/// doesn't recognise isn't listed at all, so neither the person's sidebar nor Fleetlayer would see the session.
/// Only for a pane where herdr showed no agent when the session started: where herdr recognises Gemini CLI itself,
/// its own detection stays in charge (a report takes it over). That choice is kept per pane in `herdr-panes/` under
/// the state folder. Each run waits at most a second (herdr answers in milliseconds).
fn report_to_herdr(v: &Value, state_dir: &Path) {
    let (Ok(bin), Ok(pane)) = (std::env::var("HERDR_BIN_PATH"), std::env::var("HERDR_PANE_ID")) else { return };
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") || bin.is_empty() || pane.is_empty() {
        return;
    }
    let Some(state) = herdr_state(v) else { return };
    let Some(session) = v["session_id"].as_str() else { return };
    let safe: String = pane.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let marker = state_dir.join("herdr-panes").join(safe);
    let ours = std::fs::read_to_string(&marker).is_ok_and(|s| s == session);
    if !ours {
        if v["hook_event_name"] != "SessionStart" || herdr_sees_an_agent(&bin, &pane) {
            return;
        }
        let _ = crate::files::write_atomic(&marker, session.as_bytes());
    }
    if state.is_none() {
        let _ = std::fs::remove_file(&marker);
    }
    let seq = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seq = seq.to_string();
    let mut args = vec!["pane"];
    match state {
        Some(state) => args.extend(["report-agent", &pane, "--state", state]),
        None => args.extend(["release-agent", &pane]),
    }
    args.extend(["--source", "fleetlayer:gemini", "--agent", "gemini", "--seq", &seq]);
    let Ok(mut child) = std::process::Command::new(&bin)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether herdr shows an agent in `pane` (`herdr pane get`), giving up after a second.
fn herdr_sees_an_agent(bin: &str, pane: &str) -> bool {
    let Ok(mut child) = std::process::Command::new(bin)
        .args(["pane", "get", pane])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return true;
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(1) => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                return true; // unsure: leave herdr's own detection alone
            }
        }
    }
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    serde_json::from_str::<Value>(&out)
        .ok()
        .is_none_or(|v| v.pointer("/result/pane/agent").is_some_and(|a| !a.is_null()))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracked {
    pub cwd: Option<String>,
    pub transcript: Option<String>,
    pub pid: Option<u32>,
    pub address: Option<String>,
    #[serde(skip)]
    pub prompt: Option<BlockedPrompt>,
}

#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    /// What a session waits on changed.
    Prompt(String),
    /// A session started or ended.
    Sessions,
}

/// The sessions the hooks reported, kept in `sessions.json` across restarts of the plugin.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Tracker {
    pub sessions: BTreeMap<String, Tracked>,
}

impl Tracker {
    pub fn load(path: &Path) -> Tracker {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(b) = serde_json::to_vec(self) {
            let _ = crate::files::write_atomic(path, &b);
        }
    }

    /// Drop the sessions whose agent has exited. True when any was dropped.
    pub fn prune(&mut self) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| s.pid.is_some_and(crate::files::pid_alive));
        self.sessions.len() != before
    }

    pub fn event(&mut self, kind: Kind, v: &Value, now: Timestamp) -> Change {
        let id_key = if kind == Kind::Cursor { "conversation_id" } else { "session_id" };
        let Some(id) = v[id_key].as_str().filter(|s| !s.is_empty()).map(str::to_string) else { return Change::None };
        let event = v["hook_event_name"].as_str().unwrap_or("");
        if matches!(event, "SessionEnd" | "sessionEnd") {
            return if self.sessions.remove(&id).is_some() { Change::Sessions } else { Change::None };
        }
        let fl = &v["_fleetlayer"];
        let cwd = v["cwd"]
            .as_str()
            .or_else(|| v["workspace_roots"].as_array().and_then(|r| r.first()).and_then(Value::as_str))
            .map(str::to_string);
        let new = !self.sessions.contains_key(&id);
        let s = self.sessions.entry(id.clone()).or_default();
        s.cwd = cwd.or(s.cwd.take());
        s.transcript = v["transcript_path"].as_str().map(str::to_string).or(s.transcript.take());
        s.pid = fl["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).or(s.pid);
        s.address = fl["address"].as_str().map(str::to_string).or(s.address.take());
        let prompt = match (event, v["notification_type"].as_str()) {
            ("Notification", Some("ToolPermission")) => {
                let details = &v["details"];
                let name = details["title"].as_str().unwrap_or("tool").to_string();
                let input = if details.is_object() { details.clone() } else { json!({"message": v["message"]}) };
                Some(Some(BlockedPrompt {
                    id: format!("perm-{}", now.as_millisecond()),
                    kind: PromptKind::Permission,
                    at: now,
                    source: "hook".into(),
                    questions: vec![],
                    tool: Some(PromptTool { name, input: capped(&input) }),
                    plan: None,
                    options: vec![],
                    options_from: None,
                }))
            }
            ("BeforeAgent" | "AfterAgent" | "AfterTool" | "beforeSubmitPrompt" | "stop" | "SessionStart", _) => {
                Some(None)
            }
            _ => None,
        };
        if new {
            if let Some(p) = prompt {
                s.prompt = p;
            }
            return Change::Sessions;
        }
        match prompt {
            Some(p) if p != s.prompt => {
                s.prompt = p;
                Change::Prompt(id)
            }
            _ => Change::None,
        }
    }
}

/// A tool input cut to 2 KB (it reaches everyone who may read the session): `{"truncated": true, "preview": …}`.
fn capped(input: &Value) -> Value {
    const MAX: usize = 2048;
    let text = input.to_string();
    if text.len() <= MAX {
        return input.clone();
    }
    let mut end = MAX - 64;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"truncated": true, "preview": &text[..end]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_hooks_go_in_and_out_leaving_everything_else() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, r#"{"security":{"auth":{"selectedType":"gemini-api-key"}},"hooks":{"BeforeAgent":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#).unwrap();
        let c = command(Path::new("/home/a/.local/bin/fleetlayer-harness"), Path::new("/home/a"), Kind::Gemini);
        assert_eq!(c, "\"$HOME\"/'.local/bin/fleetlayer-harness' hook gemini 2>/dev/null || true");
        assert!(edit_gemini(&p, Some(&c)).unwrap());
        assert!(!edit_gemini(&p, Some(&c)).unwrap(), "a second time changes nothing");
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(v["hooks"]["BeforeAgent"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["Notification"][0]["hooks"][0]["command"], c);
        assert!(edit_gemini(&p, None).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["hooks"], json!({"BeforeAgent": [{"hooks": [{"type": "command", "command": "mine"}]}]}));
    }

    #[test]
    fn cursor_hooks_go_in_and_out() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("hooks.json");
        let c = command(Path::new("/opt/fleetlayer-harness"), Path::new("/home/a"), Kind::Cursor);
        assert_eq!(c, "'/opt/fleetlayer-harness' hook cursor 2>/dev/null || true");
        assert!(edit_cursor(&p, Some(&c)).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["stop"][0]["command"], c);
        assert!(edit_cursor(&p, None).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["hooks"], json!({}));
        assert!(!edit_cursor(&d.path().join("none.json"), None).unwrap(), "nothing to take out, nothing created");
    }

    fn ev(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn gemini_sessions_and_their_permission_prompts() {
        let mut t = Tracker::default();
        let now = Timestamp::now();
        let me = std::process::id();
        let start = ev(&format!(
            r#"{{"session_id":"g1","transcript_path":"/t.jsonl","cwd":"/w","hook_event_name":"SessionStart","source":"startup","_fleetlayer":{{"pid":{me},"address":"w1:p5"}}}}"#
        ));
        assert_eq!(t.event(Kind::Gemini, &start, now), Change::Sessions);
        let s = &t.sessions["g1"];
        assert_eq!(
            (s.cwd.as_deref(), s.transcript.as_deref(), s.address.as_deref()),
            (Some("/w"), Some("/t.jsonl"), Some("w1:p5"))
        );
        let ask = ev(
            r#"{"session_id":"g1","hook_event_name":"Notification","notification_type":"ToolPermission","message":"Tool Confirm Shell Command requires execution","details":{"type":"exec","title":"Confirm Shell Command","command":"touch x","rootCommand":"touch"}}"#,
        );
        assert_eq!(t.event(Kind::Gemini, &ask, now), Change::Prompt("g1".into()));
        let p = t.sessions["g1"].prompt.clone().unwrap();
        assert_eq!(
            (p.kind, p.tool.as_ref().unwrap().input["command"].as_str()),
            (PromptKind::Permission, Some("touch x"))
        );
        assert_eq!(
            t.event(Kind::Gemini, &ev(r#"{"session_id":"g1","hook_event_name":"AfterTool"}"#), now),
            Change::Prompt("g1".into())
        );
        assert!(t.sessions["g1"].prompt.is_none());
        assert_eq!(
            t.event(Kind::Gemini, &ev(r#"{"session_id":"g1","hook_event_name":"AfterAgent"}"#), now),
            Change::None
        );
        assert!(!t.prune(), "its agent (this test) is alive");
        assert_eq!(
            t.event(Kind::Gemini, &ev(r#"{"session_id":"g1","hook_event_name":"SessionEnd"}"#), now),
            Change::Sessions
        );
        assert!(t.sessions.is_empty());
    }

    #[test]
    fn gemini_events_map_to_herdr_states() {
        assert_eq!(herdr_state(&ev(r#"{"hook_event_name":"BeforeAgent"}"#)), Some(Some("working")));
        assert_eq!(herdr_state(&ev(r#"{"hook_event_name":"AfterAgent"}"#)), Some(Some("idle")));
        let ask = ev(r#"{"hook_event_name":"Notification","notification_type":"ToolPermission"}"#);
        assert_eq!(herdr_state(&ask), Some(Some("blocked")));
        assert_eq!(herdr_state(&ev(r#"{"hook_event_name":"SessionEnd"}"#)), Some(None));
        assert_eq!(herdr_state(&ev(r#"{"hook_event_name":"Notification","notification_type":"Other"}"#)), None);
    }

    #[test]
    fn cursor_sessions_by_conversation_and_dead_agents_pruned() {
        let mut t = Tracker::default();
        let e = ev(
            r#"{"conversation_id":"c1","hook_event_name":"beforeSubmitPrompt","workspace_roots":["/w"],"transcript_path":"/c.jsonl","_fleetlayer":{"pid":4294967294}}"#,
        );
        assert_eq!(t.event(Kind::Cursor, &e, Timestamp::now()), Change::Sessions);
        assert_eq!(t.sessions["c1"].cwd.as_deref(), Some("/w"));
        assert!(t.prune());
        assert!(t.sessions.is_empty());
    }
}
