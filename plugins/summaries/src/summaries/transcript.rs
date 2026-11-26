//! Find transcripts, read their new records from a byte offset, and turn them into events.
//!
//! An event is one thing worth telling the summariser: a message from the user (`Human`), a message forwarded from
//! another session (`Peer`), the agent's own text (`Assistant`) or one line per tool call (`Tool`). Everything else is
//! dropped here, before any model sees it: tool output, background task and watcher notifications, scheduled
//! wake-ups, compaction summaries, slash-command chatter, plugin messages.
//!
//! Subagent transcripts (`<session>/subagents/*.jsonl`) aren't read: the parent transcript already has each
//! subagent's task (the Agent call's description) and the parent's own text relays what came back.
//!
//! Ported from claude-summarizer's `transcripts.py`. One deliberate difference: Claude Code stores every message from
//! another session with `isMeta: true`, and claude-summarizer checked `isMeta` before `origin`, so it dropped all of
//! them. Here `origin.kind == "peer"` is checked first.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use std::time::SystemTime;

use jiff::Timestamp;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Claude Code or Codex.
pub use fleetlayer_protocol::summary::AgentKind as Agent;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub path: PathBuf,
    pub agent: Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Human,
    Peer,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: Kind,
    pub text: String,
    pub ts: Timestamp,
    /// Byte offset just past the record's line.
    pub end: u64,
    /// The record's `uuid` (Claude Code only), for the catch-up's anchor.
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadResult {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub events: Vec<Event>,
    /// Byte offset just past the last complete line read.
    pub end: u64,
    /// Why the whole transcript is ignored (headless run, codex exec).
    pub skip: Option<String>,
}

/// Where transcripts live, and the home directory tool lines abbreviate to `~`.
#[derive(Debug, Clone)]
pub struct Roots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub home: String,
}

impl Roots {
    pub fn from_home(home: &Path) -> Self {
        Roots {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: home.join(".codex").join("sessions"),
            home: home.to_string_lossy().into_owned(),
        }
    }
}

/// Main transcripts modified since `cutoff`: `<projects>/*/*.jsonl` (never the `subagents/` files below a session) and
/// Codex's `<sessions>/YYYY/MM/DD/*.jsonl`.
pub fn discover(roots: &Roots, cutoff: SystemTime) -> Vec<Source> {
    let mut out = Vec::new();
    for path in glob_depth(&roots.claude_projects, 1) {
        if modified(&path).is_some_and(|m| m >= cutoff) {
            out.push(Source { path, agent: Agent::Claude });
        }
    }
    for path in glob_depth(&roots.codex_sessions, 3) {
        if modified(&path).is_some_and(|m| m >= cutoff) {
            out.push(Source { path, agent: Agent::Codex });
        }
    }
    out
}

/// `*.jsonl` files exactly `depth` directories below `root`.
fn glob_depth(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    next.push(entry.path());
                }
            }
        }
        dirs = next;
    }
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") && entry.file_type().is_ok_and(|t| t.is_file()) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

pub fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn parse_ts(value: Option<&Value>) -> Option<Timestamp> {
    value?.as_str()?.parse::<Timestamp>().ok()
}

fn mtime_ts(path: &Path) -> Timestamp {
    modified(path).and_then(|m| Timestamp::try_from(m).ok()).unwrap_or_else(Timestamp::now)
}

/// Read complete lines from `offset` on. The session ID and cwd come from the file's first records, so a read from a
/// later offset still peeks at the head for them.
pub fn read(source: &Source, offset: u64, home: &str) -> std::io::Result<ReadResult> {
    let meta = head_meta(source)?;
    let mut result =
        ReadResult { session_id: meta.session_id, cwd: meta.cwd, skip: meta.skip, end: offset, ..Default::default() };
    if result.skip.is_some() {
        result.end = std::fs::metadata(&source.path)?.len();
        return Ok(result);
    }
    let mut file = BufReader::new(File::open(&source.path)?);
    file.seek(SeekFrom::Start(offset))?;
    let mut pos = offset;
    let mut last_ts: Option<Timestamp> = None;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let n = file.read_until(b'\n', &mut raw)?;
        if n == 0 || raw.last() != Some(&b'\n') {
            break; // the end, or a line still being written
        }
        pos += n as u64;
        result.end = pos;
        let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let record = Value::Object(record);
        let ts = parse_ts(record.get("timestamp")).or(last_ts).unwrap_or_else(|| mtime_ts(&source.path));
        last_ts = Some(ts);
        let uuid = record.get("uuid").and_then(Value::as_str).map(str::to_owned);
        let parsed = match source.agent {
            Agent::Claude => claude_record(&record, home),
            Agent::Codex => codex_record(&record),
        };
        for (kind, text) in parsed {
            let text = text.trim();
            if !text.is_empty() {
                result.events.push(Event { kind, text: text.to_owned(), ts, end: pos, uuid: uuid.clone() });
            }
        }
    }
    Ok(result)
}

#[derive(Debug, Default)]
struct HeadMeta {
    session_id: Option<String>,
    cwd: Option<String>,
    skip: Option<String>,
}

fn head_meta(source: &Source) -> std::io::Result<HeadMeta> {
    let mut meta = HeadMeta::default();
    let mut file = BufReader::new(File::open(&source.path)?);
    let mut raw = Vec::new();
    if source.agent == Agent::Codex {
        file.read_until(b'\n', &mut raw)?;
        let payload = serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("payload").cloned())
            .filter(Value::is_object)
            .unwrap_or(Value::Null);
        meta.session_id = str_field(&payload, "id").or_else(|| str_field(&payload, "session_id"));
        meta.cwd = str_field(&payload, "cwd");
        let source_ok = matches!(payload.get("source").and_then(Value::as_str), Some("cli" | "vscode"));
        if payload.get("originator").and_then(Value::as_str) == Some("codex_exec") || !source_ok {
            meta.skip = Some("codex exec run (a review a Claude session started)".into());
        }
        return Ok(meta);
    }
    meta.session_id = source.path.file_stem().map(|s| s.to_string_lossy().into_owned());
    for _ in 0..200 {
        raw.clear();
        if file.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let Ok(record @ Value::Object(_)) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        if meta.cwd.is_none() {
            meta.cwd = str_field(&record, "cwd").filter(|s| !s.is_empty());
        }
        if let Some(entrypoint) = str_field(&record, "entrypoint").filter(|s| !s.is_empty()) {
            if entrypoint != "cli" {
                meta.skip = Some(format!("headless run ({entrypoint})"));
            }
            break;
        }
    }
    Ok(meta)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Python truthiness, for the `a or b or c` fallbacks ported from claude-summarizer.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn first_truthy<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().filter_map(|k| v.get(*k)).find(|x| truthy(x))
}

// ---------------------------------------------------------------- Claude Code

pub(crate) const SKIP_ORIGINS: &[&str] = &["task-notification", "plugin", "auto-continuation"];
pub const NOISE_COMMANDS: &[&str] = &[
    "/mcp",
    "/model",
    "/login",
    "/logout",
    "/compact",
    "/clear",
    "/list-agents",
    "/feedback",
    "/effort",
    "/usage",
    "/usage-credits",
    "/autocompact",
    "/config",
    "/status",
    "/cost",
    "/resume",
    "/tui",
    "/reload-plugins",
    "/plugin",
    "/permissions",
    "/doctor",
    "/help",
    "/exit",
    "/fast",
    "/context",
    "/memory",
    "/hooks",
    "/ide",
    "/terminal-setup",
    "/theme",
    "/vim",
    "/statusline",
    "/agents",
    "/rename",
    "/export",
    "/release-notes",
    "/upgrade",
    "/btw",
];
const QUIET_TOOLS: &[&str] =
    &["Read", "Grep", "Glob", "ToolSearch", "TaskOutput", "TodoWrite", "TaskList", "TaskGet", "LSP"];

pub(crate) static SYSTEM_REMINDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap());
pub(crate) static COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-name>(.*?)</command-name>").unwrap());
pub(crate) static COMMAND_ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-args>(.*?)</command-args>").unwrap());
pub(crate) static PEER_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:cross-session-message|agent-message)[^>]*>(.*?)</(?:cross-session-message|agent-message)>")
        .unwrap()
});

/// herdr's default `[session.auto_continue]` message. `{reason}` is why the session stopped.
pub const HERDR_AUTO_CONTINUE: &str = "Please continue where you left off. ({reason})";
static AUTO_CONTINUE: OnceLock<String> = OnceLock::new();

/// The auto-continue message herdr is configured with, when it isn't the default. Set once, at the node's start.
pub fn set_auto_continue_message(template: &str) {
    let _ = AUTO_CONTINUE.set(template.to_string());
}

/// herdr types its auto-continue message into a session after a restart or a usage limit, the way a person would.
/// It isn't a person's message: it holds nothing worth summarising and doesn't move the catch-up's anchor.
pub fn is_auto_continue(text: &str) -> bool {
    let template = AUTO_CONTINUE.get().map(String::as_str).unwrap_or(HERDR_AUTO_CONTINUE);
    matches_template(template, text.trim())
}

fn matches_template(template: &str, text: &str) -> bool {
    match template.split_once("{reason}") {
        Some((before, after)) => {
            text.len() > before.len() + after.len()
                && text.starts_with(before)
                && text.ends_with(after)
                && !text[before.len()..text.len() - after.len()].contains('\n')
        }
        None => text == template.trim(),
    }
}

fn user_text(text: &str) -> Option<String> {
    let cleaned = SYSTEM_REMINDER.replace_all(text, "");
    let text = cleaned.trim();
    if text.is_empty() || text.starts_with("This session is being continued from a previous conversation") {
        return None;
    }
    if is_auto_continue(text) {
        return None;
    }
    if ["<local-command-", "<artifact-content-authored-by-others", "<task-notification"]
        .iter()
        .any(|p| text.starts_with(p))
    {
        return None;
    }
    if text.starts_with("[Request interrupted by user") {
        return None;
    }
    if let Some(m) = COMMAND.captures(text) {
        let mut name = m[1].trim().to_owned();
        if !name.starts_with('/') {
            name.insert(0, '/');
        }
        if NOISE_COMMANDS.contains(&name.as_str()) {
            return None;
        }
        let args = COMMAND_ARGS.captures(text).map(|a| a[1].trim().to_owned()).unwrap_or_default();
        return Some(format!("{name} {args}").trim().to_owned());
    }
    Some(text.to_owned())
}

pub fn claude_record(record: &Value, home: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let kind = record.get("type").and_then(Value::as_str);
    let content = record.get("message").and_then(|m| m.get("content"));
    match kind {
        Some("user") => {
            if record.get("isCompactSummary").is_some_and(truthy) {
                return out;
            }
            let origin = record.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str);
            if origin.is_some_and(|o| SKIP_ORIGINS.contains(&o)) {
                return out;
            }
            // Messages from other sessions carry isMeta too, so origin goes first.
            if origin != Some("peer") && record.get("isMeta").is_some_and(truthy) {
                return out;
            }
            let texts: Vec<String> = match content {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(blocks)) => {
                    if blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")) {
                        return out;
                    }
                    let mut texts: Vec<String> = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .map(|b| b.get("text").and_then(Value::as_str).unwrap_or("").to_owned())
                        .collect();
                    if blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("image")) {
                        texts.push("[image attached]".into());
                    }
                    texts
                }
                _ => return out,
            };
            let joined = texts.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
            if origin == Some("peer") {
                let body = PEER_BODY.captures(&joined).map(|m| m[1].to_owned()).unwrap_or_else(|| joined.clone());
                let body = SYSTEM_REMINDER.replace_all(&body, "").trim().to_owned();
                if !body.is_empty() {
                    out.push((Kind::Peer, body));
                }
                return out;
            }
            if let Some(text) = user_text(&joined) {
                out.push((Kind::Human, text));
            }
        }
        Some("assistant") => {
            let Some(Value::Array(blocks)) = content else {
                return out;
            };
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        out.push((Kind::Assistant, block.get("text").and_then(Value::as_str).unwrap_or("").to_owned()))
                    }
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = block
                            .get("input")
                            .filter(|v| truthy(v))
                            .cloned()
                            .unwrap_or(Value::Object(Default::default()));
                        if let Some(line) = tool_line(name, &input, home) {
                            out.push((Kind::Tool, line));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// JSON the way Python's `json.dumps(value, ensure_ascii=False)` writes it (", " and ": " separators), so tool lines
/// match claude-summarizer's.
pub fn python_json(value: &Value) -> String {
    struct PyFormatter;
    impl serde_json::ser::Formatter for PyFormatter {
        fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFormatter);
    serde::Serialize::serialize(value, &mut ser).expect("serialising a Value can't fail");
    String::from_utf8(buf).expect("serde_json writes UTF-8")
}

/// Whitespace collapsed, cut to `n` characters with an ellipsis.
pub fn short(value: &Value, n: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => python_json(other),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= n { text } else { text.chars().take(n - 1).collect::<String>() + "…" }
}

fn arg<'a>(args: &'a Value, keys: &[&str]) -> &'a Value {
    static EMPTY: Value = Value::String(String::new());
    first_truthy(args, keys).unwrap_or(&EMPTY)
}

pub(crate) fn tool_line(name: &str, args: &Value, home: &str) -> Option<String> {
    if QUIET_TOOLS.contains(&name) {
        return None;
    }
    let plain = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => python_json(other),
    };
    Some(match name {
        "Bash" => format!("Bash: {}", short(arg(args, &["description", "command"]), 140)),
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => {
            let path = plain(arg(args, &["file_path", "notebook_path"]));
            let path = if home.is_empty() { path } else { path.replace(home, "~") };
            format!("{name} {path}")
        }
        "Agent" | "Task" => format!("Agent: {}", short(arg(args, &["description", "prompt"]), 140)),
        "Skill" => {
            let skill = args.get("skill").map(plain).unwrap_or_else(|| "None".into());
            format!("Skill {skill} {}", short(arg(args, &["args"]), 80)).trim().to_owned()
        }
        "SendMessage" => format!(
            "SendMessage to {}: {}",
            args.get("to").map(plain).unwrap_or_else(|| "None".into()),
            short(args.get("message").unwrap_or(&Value::String(String::new())), 120)
        ),
        "WebSearch" | "WebFetch" => format!("{name}: {}", short(arg(args, &["query", "url"]), 120)),
        _ => format!("{name}: {}", short(args, 120)),
    })
}

// ---------------------------------------------------------------- Codex

const CODEX_SKIP_PREFIXES: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context",
    "<user_instructions",
    "<turn_aborted",
    "<permissions",
    "<model_switch",
    "<multi_agent",
    "<user_shell_command",
];

pub fn codex_record(record: &Value) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return out;
    }
    let p = record.get("payload").cloned().unwrap_or(Value::Null);
    match p.get("type").and_then(Value::as_str) {
        Some("message") => {
            let parts = p.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
            let text = parts
                .iter()
                .filter(|c| c.is_object())
                .map(|c| c.get("text").and_then(Value::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            match p.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let text = text.trim();
                    if !text.is_empty()
                        && !CODEX_SKIP_PREFIXES.iter().any(|x| text.starts_with(x))
                        && !is_auto_continue(text)
                    {
                        out.push((Kind::Human, text.to_owned()));
                    }
                }
                Some("assistant") => out.push((Kind::Assistant, text)),
                _ => {}
            }
        }
        Some(kind @ ("function_call" | "custom_tool_call" | "local_shell_call")) => {
            let args = arg(&p, &["arguments", "input", "action"]);
            let name = p.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(kind);
            out.push((Kind::Tool, format!("{name}: {}", short(args, 140))));
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOME: &str = "/Users/sam";

    fn kinds(v: Value) -> Vec<(Kind, String)> {
        claude_record(&v, HOME)
    }

    #[test]
    fn peer_messages_are_kept_although_marked_meta() {
        let r = json!({"type": "user", "isMeta": true, "origin": {"kind": "peer", "name": "esign ui"},
            "message": {"role": "user", "content": "Another Claude session sent a message:\n<cross-session-message from=\"uds:/tmp/x.sock\">Heads-up: signed PDFs gain a caption.<system-reminder>ignore</system-reminder></cross-session-message>"}});
        assert_eq!(kinds(r), vec![(Kind::Peer, "Heads-up: signed PDFs gain a caption.".into())]);
    }

    #[test]
    fn meta_records_and_skipped_origins_are_dropped() {
        for r in [
            json!({"type": "user", "isMeta": true, "message": {"content": "Continue sc-1: check the batch"}}),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "This session is being continued"}}),
            json!({"type": "user", "origin": {"kind": "task-notification"}, "message": {"content": "<task-notification>x</task-notification>"}}),
            json!({"type": "user", "origin": {"kind": "plugin"}, "message": {"content": "The account-rotator plugin sent a message"}}),
            json!({"type": "user", "isMeta": true, "origin": {"kind": "auto-continuation"}, "message": {"content": "Your claude.ai usage limit has reset."}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "1\tfile"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}),
            json!({"type": "user", "message": {"content": "<local-command-stdout>ok</local-command-stdout>"}}),
            json!({"type": "user", "message": {"content": "<command-name>/model</command-name>\n<command-message>model</command-message>"}}),
            json!({"type": "user", "message": {"content": "<system-reminder>only a reminder</system-reminder>"}}),
            json!({"type": "system", "subtype": "compact_boundary", "content": "Conversation compacted"}),
            json!({"type": "ai-title", "title": "x"}),
        ] {
            assert_eq!(kinds(r.clone()), vec![], "{r}");
        }
    }

    #[test]
    fn human_messages_commands_and_images() {
        let r = json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "<system-reminder>r</system-reminder>  Pick up sc-6413.  "}});
        assert_eq!(kinds(r), vec![(Kind::Human, "Pick up sc-6413.".into())]);
        let r = json!({"type": "user", "message": {"content": "<command-message>wrap-up-session</command-message>\n<command-name>/wrap-up-session</command-name>\n<command-args>now</command-args>"}});
        assert_eq!(kinds(r), vec![(Kind::Human, "/wrap-up-session now".into())]);
        let r = json!({"type": "user", "message": {"content": "<command-name>loop</command-name>"}});
        assert_eq!(kinds(r), vec![(Kind::Human, "/loop".into())]);
        let r = json!({"type": "user", "message": {"content": [{"type": "text", "text": "look"}, {"type": "image", "source": {}}]}});
        assert_eq!(kinds(r), vec![(Kind::Human, "look\n[image attached]".into())]);
    }

    #[test]
    fn assistant_text_and_tool_lines() {
        let r = json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "Done."},
            {"type": "tool_use", "name": "Read", "input": {"file_path": "/x"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "ls   -la\n/tmp", "description": ""}},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/Users/sam/code/a.rs"}},
            {"type": "tool_use", "name": "Skill", "input": {"skill": "pr-review"}},
            {"type": "tool_use", "name": "SendMessage", "input": {"to": "ci", "message": "hi"}},
            {"type": "tool_use", "name": "mcp__x__y", "input": {"b": 1, "a": [1, 2]}},
        ]}});
        assert_eq!(
            kinds(r),
            vec![
                (Kind::Assistant, "Done.".into()),
                (Kind::Tool, "Bash: ls -la /tmp".into()),
                (Kind::Tool, "Edit ~/code/a.rs".into()),
                (Kind::Tool, "Skill pr-review".into()),
                (Kind::Tool, "SendMessage to ci: hi".into()),
                (Kind::Tool, r#"mcp__x__y: {"b": 1, "a": [1, 2]}"#.into()),
            ]
        );
    }

    #[test]
    fn short_cuts_with_an_ellipsis() {
        assert_eq!(short(&json!("abcdef"), 4), "abc…");
        assert_eq!(short(&json!("a  b\n c"), 10), "a b c");
        assert_eq!(short(&json!("ééé"), 3), "ééé");
    }

    #[test]
    fn herdrs_auto_continue_prompt_is_not_a_persons_message() {
        for reason in ["herdr restarted", "the computer restarted", "your usage limit reset"] {
            let text = format!("Please continue where you left off. ({reason})");
            assert!(kinds(json!({"type": "user", "message": {"content": text}})).is_empty(), "{reason}");
            let codex = json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]}});
            assert_eq!(codex_record(&codex), vec![]);
        }
        let person = "Please continue where you left off. (herdr restarted) And then fix the build.";
        assert_eq!(
            kinds(json!({"type": "user", "message": {"content": person}})).len(),
            1,
            "a person's words after it"
        );
        assert!(matches_template("Keep going ({reason}), thanks", "Keep going (herdr restarted), thanks"));
        assert!(!matches_template("Keep going ({reason}), thanks", "Keep going (), thanks"));
        assert!(matches_template("carry on", "carry on"));
        assert!(!matches_template(HERDR_AUTO_CONTINUE, "Please continue where you left off. (a\nb)"));
    }

    #[test]
    fn codex_records() {
        let user = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the build"}]}});
        assert_eq!(codex_record(&user), vec![(Kind::Human, "fix the build".into())]);
        let env = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "<environment_context>x"}]}});
        assert_eq!(codex_record(&env), vec![]);
        let call = json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"}});
        assert_eq!(codex_record(&call), vec![(Kind::Tool, "shell: {\"cmd\":\"ls\"}".into())]);
        let local =
            json!({"type": "response_item", "payload": {"type": "local_shell_call", "action": {"command": ["ls"]}}});
        assert_eq!(codex_record(&local), vec![(Kind::Tool, r#"local_shell_call: {"command": ["ls"]}"#.into())]);
        assert_eq!(codex_record(&json!({"type": "event_msg", "payload": {}})), vec![]);
    }
}
