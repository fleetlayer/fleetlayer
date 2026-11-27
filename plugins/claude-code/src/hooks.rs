//! What a Claude Code session is waiting for, from Claude Code's own hooks rather than its screen.
//!
//! The plugin installs a few hooks in `~/.claude/settings.json` (every other key and hook kept, the file replaced in
//! one rename, through a symbolic link to the file it names). Each runs the plugin's own binary by a path that doesn't
//! depend on the session's PATH ([`command`]), asynchronously, so it can never hold Claude up or change what it does: the hook passes its JSON to the plugin's socket and exits 0 with no output, and exits 0 too when the
//! plugin isn't running or isn't installed on that machine.
//!
//! From those events the [`Tracker`] keeps, per session, the prompt it's blocked on (an `AskUserQuestion` menu, a
//! permission prompt, a plan to approve) and the background work Claude Code reported still running when it last
//! stopped.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fleetlayer_protocol::ext::{BackgroundTask, BlockedPrompt, PromptKind, PromptOption, PromptQuestion, PromptTool};
use jiff::Timestamp;
use serde_json::{Map, Value, json};

/// What marks a hook as this plugin's, so installing again replaces it and nothing else.
const MARK: &str = "fleetlayer-claude-code";

/// The hook's command: this plugin's own binary by a path that doesn't depend on the session's PATH (which can hold
/// relative entries such as `./node_modules/.bin`, so a cloned repo could put its own program first), quiet and
/// successful whatever happens. Under the home folder it's written from `$HOME`, which the shell expands when the hook
/// runs, so a settings file shared between machines works on each; elsewhere it's the absolute path.
pub fn command(exe: &Path, home: &Path) -> String {
    let quote = |p: &str| p.replace('\'', "'\\''");
    match exe.strip_prefix(home) {
        Ok(rel) if !home.as_os_str().is_empty() => {
            format!("\"$HOME\"/'{}' hook 2>/dev/null || true", quote(&rel.to_string_lossy()))
        }
        _ => format!("'{}' hook 2>/dev/null || true", quote(&exe.to_string_lossy())),
    }
}

/// The events it listens to, with their tool matcher (None: the event has none).
const EVENTS: &[(&str, Option<&str>)] = &[
    ("PreToolUse", Some("AskUserQuestion|ExitPlanMode")),
    ("PermissionRequest", Some("*")),
    ("PostToolUse", Some("*")),
    ("PostToolUseFailure", Some("*")),
    ("PermissionDenied", Some("*")),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("SessionEnd", None),
];

/// The socket the plugin listens on and the hook writes to: `$FLEETLAYER_CLAUDE_HOOKS_SOCKET`, else under
/// `$XDG_RUNTIME_DIR`, else `~/.local/state/fleetlayer/`.
pub fn socket_path(home: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("FLEETLAYER_CLAUDE_HOOKS_SOCKET") {
        return PathBuf::from(p);
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("fleetlayer/claude-hooks.sock"),
        None => home.join(".local/state/fleetlayer/claude-hooks.sock"),
    }
}

/// `fleetlayer-claude-code hook`: pass the event on stdin to the plugin, never failing and never printing.
pub fn run_hook() {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut input);
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket_path(&home)) {
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        let line: Vec<u8> = input.into_iter().filter(|b| *b != b'\n').chain(std::iter::once(b'\n')).collect();
        let _ = s.write_all(&line);
    }
}

/// Make sure the hooks, running `command`, are in `settings` (Claude Code's `settings.json`). True when the file
/// changed.
pub fn install(settings: &Path, command: &str) -> Result<bool, String> {
    edit(settings, Some(command))
}

/// Take the plugin's hooks out of `settings` again, leaving everyone else's.
pub fn uninstall(settings: &Path) -> Result<bool, String> {
    edit(settings, None)
}

fn ours(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hs| {
        hs.iter().any(|h| h["command"].as_str().is_some_and(|c| c.contains(MARK) && c.contains(" hook")))
    })
}

fn edit(settings: &Path, want: Option<&str>) -> Result<bool, String> {
    // Through a symlink to the file it names, so a synced or linked settings file stays linked.
    let path = std::fs::canonicalize(settings).unwrap_or_else(|_| settings.to_path_buf());
    let before: Value = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{} isn't JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if want.is_none() {
                return Ok(false);
            }
            Value::Object(Map::new())
        }
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let mut after = before.clone();
    let root = after.as_object_mut().ok_or_else(|| format!("{} isn't a JSON object", path.display()))?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or_else(|| format!("hooks in {} isn't an object", path.display()))?;
    for (event, matcher) in EVENTS {
        let list = hooks.entry(*event).or_insert_with(|| Value::Array(vec![]));
        let list = list.as_array_mut().ok_or_else(|| format!("hooks.{event} in {} isn't a list", path.display()))?;
        list.retain(|e| !ours(e));
        if let Some(command) = want {
            let mut entry = json!({"hooks": [{"type": "command", "command": command, "async": true, "timeout": 5}]});
            if let Some(m) = matcher {
                entry["matcher"] = json!(m);
            }
            list.push(entry);
        }
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    if root.get("hooks").and_then(Value::as_object).is_some_and(Map::is_empty) {
        root.remove("hooks");
    }
    if after == before {
        return Ok(false);
    }
    crate::trust::write_atomic(&path, &serde_json::to_vec_pretty(&after).map_err(|e| e.to_string())?)?;
    Ok(true)
}

/// Per session: the prompt it's blocked on and the background work it reported.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Waiting {
    pub prompt: Option<BlockedPrompt>,
    pub background: Option<Vec<BackgroundTask>>,
}

#[derive(Debug, Default)]
pub struct Tracker {
    sessions: HashMap<String, Waiting>,
}

impl Tracker {
    pub fn get(&self, session: &str) -> Option<&Waiting> {
        self.sessions.get(session)
    }

    /// One hook event. Returns the session it changed, if it changed one.
    pub fn event(&mut self, v: &Value, now: Timestamp) -> Option<String> {
        let session = v["session_id"].as_str()?.to_string();
        let event = v["hook_event_name"].as_str()?;
        let tool = v["tool_name"].as_str().unwrap_or("");
        let tool_use = v["tool_use_id"].as_str();
        let before = self.sessions.get(&session).cloned().unwrap_or_default();
        let mut w = before.clone();
        match event {
            "PreToolUse" if tool == "AskUserQuestion" => {
                w.prompt = Some(BlockedPrompt {
                    id: tool_use.map(str::to_string).unwrap_or_else(|| made_up_id(&session, v, now)),
                    kind: PromptKind::Question,
                    at: now,
                    source: "hook".into(),
                    questions: questions(&v["tool_input"]),
                    tool: None,
                    plan: None,
                    options: vec![],
                    options_from: None,
                });
            }
            "PreToolUse" if tool == "ExitPlanMode" => {
                w.prompt = Some(BlockedPrompt {
                    id: tool_use.map(str::to_string).unwrap_or_else(|| made_up_id(&session, v, now)),
                    kind: PromptKind::Plan,
                    at: now,
                    source: "hook".into(),
                    questions: vec![],
                    tool: None,
                    plan: v["tool_input"]["plan"].as_str().map(str::to_string),
                    options: vec![],
                    options_from: None,
                });
            }
            "PermissionRequest" => {
                // A question or a plan already showing is that same dialog.
                let same = w.prompt.as_ref().is_some_and(|p| p.kind != PromptKind::Permission)
                    && matches!(tool, "AskUserQuestion" | "ExitPlanMode");
                if !same {
                    w.prompt = Some(BlockedPrompt {
                        id: made_up_id(&session, v, now),
                        kind: PromptKind::Permission,
                        at: now,
                        source: "hook".into(),
                        questions: vec![],
                        tool: Some(PromptTool { name: tool.to_string(), input: capped(&v["tool_input"]) }),
                        plan: None,
                        options: vec![],
                        options_from: None,
                    });
                }
            }
            "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
                let done = w.prompt.as_ref().is_some_and(|p| {
                    tool_use.is_some_and(|t| t == p.id)
                        || (p.kind == PromptKind::Permission && p.tool.as_ref().is_some_and(|t| t.name == tool))
                });
                if done {
                    w.prompt = None;
                }
            }
            "UserPromptSubmit" => w.prompt = None,
            "Stop" => {
                w.prompt = None;
                w.background = Some(background(&v["background_tasks"]));
            }
            "SessionEnd" => {
                self.sessions.remove(&session);
                return (before != Waiting::default()).then_some(session);
            }
            _ => {}
        }
        if w == before {
            return None;
        }
        self.sessions.insert(session.clone(), w);
        Some(session)
    }
}

/// The most of a permission prompt's tool input passed on, as JSON bytes: it reaches the control plane and everyone
/// who may read the session.
pub const TOOL_INPUT_MAX: usize = 2048;

/// A tool input cut to [`TOOL_INPUT_MAX`]: `{"truncated": true, "preview": "<the start of its JSON>"}` when longer.
fn capped(input: &Value) -> Value {
    let text = input.to_string();
    if text.len() <= TOOL_INPUT_MAX {
        return input.clone();
    }
    let mut end = TOOL_INPUT_MAX - 64;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"truncated": true, "preview": &text[..end]})
}

/// An ID for a prompt the hook gives none for: new for every prompt.
fn made_up_id(session: &str, v: &Value, now: Timestamp) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut h);
    v["tool_name"].as_str().hash(&mut h);
    v["tool_input"].to_string().hash(&mut h);
    now.as_nanosecond().hash(&mut h);
    format!("p-{:016x}", h.finish())
}

/// `AskUserQuestion`'s questions, its options numbered as Claude Code shows them, then "Type something".
fn questions(input: &Value) -> Vec<PromptQuestion> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let options: Vec<PromptOption> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, o)| PromptOption {
                    n: i as u32 + 1,
                    label: o["label"].as_str().unwrap_or("").to_string(),
                    description: o["description"].as_str().map(str::to_string),
                })
                .collect();
            PromptQuestion {
                header: q["header"].as_str().unwrap_or("").to_string(),
                question: q["question"].as_str().unwrap_or("").to_string(),
                multi_select: q["multiSelect"].as_bool().unwrap_or(false),
                other: Some(options.len() as u32 + 1),
                options,
            }
        })
        .collect()
}

/// Stop's `background_tasks`, the ones still going.
fn background(tasks: &Value) -> Vec<BackgroundTask> {
    tasks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["status"].as_str().is_none_or(|s| matches!(s, "running" | "pending" | "in_progress")))
        .map(|t| BackgroundTask {
            kind: t["type"].as_str().unwrap_or("task").to_string(),
            id: t["id"].as_str().map(str::to_string).unwrap_or_else(|| t["id"].to_string()),
            description: t["description"].as_str().or(t["command"].as_str()).map(str::to_string),
            status: t["status"].as_str().map(str::to_string),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, extra: Value) -> Value {
        let mut v = json!({"session_id": "s1", "hook_event_name": event});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    }

    #[test]
    fn a_question_shows_until_its_tool_finishes() {
        let mut t = Tracker::default();
        let now = Timestamp::now();
        let ask = ev(
            "PreToolUse",
            json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_1", "tool_input": {"questions": [
                {"question": "Which colour?", "header": "Colour", "options": [{"label": "Red", "description": "r"}, {"label": "Blue"}]},
                {"question": "Which sizes?", "header": "Sizes", "multiSelect": true, "options": [{"label": "S"}, {"label": "M"}]}
            ]}}),
        );
        assert_eq!(t.event(&ask, now).as_deref(), Some("s1"));
        let p = t.get("s1").unwrap().prompt.clone().unwrap();
        assert_eq!((p.id.as_str(), p.kind, p.source.as_str()), ("toolu_1", PromptKind::Question, "hook"));
        assert_eq!(p.questions[0].options[1], PromptOption { n: 2, label: "Blue".into(), description: None });
        assert_eq!((p.questions[0].other, p.questions[1].multi_select), (Some(3), true));
        // Its own permission request is the same dialog; another tool's PostToolUse doesn't clear it.
        assert_eq!(t.event(&ev("PermissionRequest", json!({"tool_name": "AskUserQuestion"})), now), None);
        assert_eq!(t.event(&ev("PostToolUse", json!({"tool_name": "Read", "tool_use_id": "toolu_0"})), now), None);
        assert!(
            t.event(&ev("PostToolUse", json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_1"})), now)
                .is_some()
        );
        assert_eq!(t.get("s1").unwrap().prompt, None);
    }

    #[test]
    fn a_permission_prompt_and_a_plan_clear_when_answered() {
        let mut t = Tracker::default();
        let now = Timestamp::now();
        let ask = ev("PermissionRequest", json!({"tool_name": "Bash", "tool_input": {"command": "touch x"}}));
        t.event(&ask, now);
        let p = t.get("s1").unwrap().prompt.clone().unwrap();
        assert_eq!(p.kind, PromptKind::Permission);
        assert!(p.id.starts_with("p-"));
        assert_eq!(p.tool.unwrap().input["command"], "touch x");
        t.event(&ev("PostToolUse", json!({"tool_name": "Bash", "tool_use_id": "toolu_9"})), now);
        assert_eq!(t.get("s1").unwrap().prompt, None);
        // A second request for the same command is a new prompt with a new ID.
        t.event(&ask, now);
        let first = t.get("s1").unwrap().prompt.clone().unwrap().id;
        t.event(&ev("PermissionDenied", json!({"tool_name": "Bash"})), now);
        t.event(&ask, now + jiff::SignedDuration::from_secs(1));
        assert_ne!(t.get("s1").unwrap().prompt.clone().unwrap().id, first);
        t.event(&ev("UserPromptSubmit", json!({"prompt": "no"})), now);
        assert_eq!(t.get("s1").unwrap().prompt, None);

        let plan = ev(
            "PreToolUse",
            json!({"tool_name": "ExitPlanMode", "tool_use_id": "toolu_p", "tool_input": {"plan": "1. Do it"}}),
        );
        t.event(&plan, now);
        let p = t.get("s1").unwrap().prompt.clone().unwrap();
        assert_eq!((p.kind, p.plan.as_deref()), (PromptKind::Plan, Some("1. Do it")));
        t.event(
            &ev(
                "Stop",
                json!({"background_tasks": [
                    {"id": "b1", "type": "bash", "status": "running", "description": "cargo test"},
                    {"id": "b2", "type": "agent", "status": "completed"}
                ]}),
            ),
            now,
        );
        let w = t.get("s1").unwrap();
        assert_eq!(w.prompt, None);
        assert_eq!(w.background.as_ref().unwrap().len(), 1);
        assert_eq!(w.background.as_ref().unwrap()[0].description.as_deref(), Some("cargo test"));
        assert!(t.event(&ev("SessionEnd", json!({"reason": "other"})), now).is_some());
        assert!(t.get("s1").is_none());
    }

    #[test]
    fn installing_keeps_every_other_hook_and_runs_twice_to_the_same_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        let theirs = json!({"theme": "dark", "hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/guard", "timeout": 15}]}
        ]}});
        std::fs::write(&p, serde_json::to_vec(&theirs).unwrap()).unwrap();
        let home = Path::new("/home/a");
        let cmd = command(Path::new("/opt/fleet layer/bin/fleetlayer-claude-code"), home);
        assert_eq!(cmd, "'/opt/fleet layer/bin/fleetlayer-claude-code' hook 2>/dev/null || true");
        assert!(install(&p, &cmd).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "/usr/local/bin/guard");
        assert_eq!(pre[1]["matcher"], "AskUserQuestion|ExitPlanMode");
        assert_eq!(pre[1]["hooks"][0]["async"], true);
        assert!(v["hooks"]["Stop"][0].get("matcher").is_none());
        assert_eq!(pre[1]["hooks"][0]["command"], cmd);
        assert!(!install(&p, &cmd).unwrap(), "already there: unchanged");
        // A moved binary replaces its own hooks, not anyone else's.
        assert!(install(&p, &command(Path::new("/usr/local/bin/fleetlayer-claude-code"), home)).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert!(uninstall(&p).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v, theirs, "back to exactly what it was");
    }

    #[test]
    fn a_linked_settings_file_stays_linked() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.json");
        std::fs::write(&real, "{}").unwrap();
        let link = d.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        install(&link, &command(Path::new("/x/fleetlayer-claude-code"), Path::new("/home/a"))).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let v: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
        assert!(v["hooks"]["Stop"].is_array());
    }

    /// The command never depends on the session's PATH: it's absolute, or anchored at `$HOME`. An older bare entry is
    /// replaced on upgrade.
    #[test]
    fn the_command_is_absolute_or_home_anchored_and_replaces_the_old_bare_one() {
        let home = Path::new("/home/a");
        let under = command(Path::new("/home/a/.local/bin/fleetlayer-claude-code"), home);
        assert_eq!(under, "\"$HOME\"/'.local/bin/fleetlayer-claude-code' hook 2>/dev/null || true");
        for c in [under.clone(), command(Path::new("/usr/local/bin/fleetlayer-claude-code"), home)] {
            assert!(c.starts_with("'/") || c.starts_with("\"$HOME\"/"), "{c}");
        }
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        let old = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "fleetlayer-claude-code hook 2>/dev/null || true"}]}]}});
        std::fs::write(&p, serde_json::to_vec(&old).unwrap()).unwrap();
        install(&p, &under).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], under);
    }

    #[test]
    fn a_long_tool_input_is_cut() {
        let long = json!({"command": "x".repeat(5000)});
        let c = capped(&long);
        assert_eq!(c["truncated"], true);
        assert!(c.to_string().len() <= TOOL_INPUT_MAX);
        assert_eq!(capped(&json!({"command": "ls"})), json!({"command": "ls"}));
    }
}
