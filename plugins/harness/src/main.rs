//! fleetlayer-harness: Fleetlayer's harness plugins for pi, OpenCode, Gemini CLI and Cursor's CLI (`docs/protocol.md`
//! section 5, `docs/harnesses.md`). One binary, one plugin process per harness: `fleetlayer-harness <harness>`.
//!
//! - **pi** and **OpenCode** get a small shim inside the agent (`shims/`: pi's extension, OpenCode's plugin), so a
//!   message reaches a running session through the agent's own API and the person sees it in their tab, and what a
//!   session waits on is reported as it happens (`shim.rs`).
//! - **Gemini CLI** and **Cursor's CLI** report through their own hooks (`hooks.rs`). Neither takes a message from
//!   outside, so the node types messages into their terminals.
//!
//! For all four it lists the sessions it knows (with the pane each runs in, so the session manager's unnamed entry
//! gets the agent's own ID), writes their transcripts in one shape (`transcript.rs`), finds where a stored session
//! worked so it can be resumed by its ID, and gets a folder ready before a session starts there.
//!
//! Config (all optional): `dir`, the harness's own folder (pi: `~/.pi/agent` or `$PI_CODING_AGENT_DIR`; OpenCode:
//! `~/.config/opencode`; Gemini CLI: `~/.gemini`; Cursor: `~/.cursor`); `install: false` takes the shim or hooks out
//! again; `program`, the `opencode` command for transcripts (default `opencode`).

mod files;
mod hooks;
mod shim;
mod transcript;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{
    self, Capabilities, Implementation, InitializeRequest, InitializeResponse, ListSessionsResponse, SessionInfo,
    SessionInfoTag, SessionInfoUpdate, SessionNotification, SessionUpdate,
};
use fleetlayer_protocol::ext::{
    self, BlockedPrompt, Deliver, Delivered, HarnessRole, NodeMeta, PluginMeta, PluginStatusReport, PromptKind,
    PromptOption, PromptQuestion, PromptTool, Roles, SessionMeta, TranscriptFile, Via, meta_key,
};
use fleetlayer_protocol::rpc::{RpcError, code};
use serde::Deserialize;
use serde_json::{Value, json};

const PI_SHIM: &str = include_str!("../shims/pi.ts");
const OPENCODE_SHIM: &str = include_str!("../shims/opencode.js");

/// The most transcripts listed for a search, newest first.
const TRANSCRIPTS_MAX: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pi,
    Opencode,
    Gemini,
    Cursor,
}

impl Kind {
    fn parse(s: &str) -> Option<Kind> {
        match s {
            "pi" => Some(Kind::Pi),
            "opencode" => Some(Kind::Opencode),
            "gemini" => Some(Kind::Gemini),
            "cursor" => Some(Kind::Cursor),
            _ => None,
        }
    }

    /// The harness's name in Fleetlayer and herdr.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Pi => "pi",
            Kind::Opencode => "opencode",
            Kind::Gemini => "gemini",
            Kind::Cursor => "cursor",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Kind::Pi => "pi",
            Kind::Opencode => "OpenCode",
            Kind::Gemini => "Gemini CLI",
            Kind::Cursor => "Cursor",
        }
    }

    fn uses_shim(self) -> bool {
        matches!(self, Kind::Pi | Kind::Opencode)
    }

    fn default_dir(self, home: &Path) -> PathBuf {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        match self {
            Kind::Pi => env("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi/agent")),
            Kind::Opencode => env("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join("opencode"),
            Kind::Gemini => home.join(".gemini"),
            Kind::Cursor => env("CURSOR_CONFIG_DIR").unwrap_or_else(|| home.join(".cursor")),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    dir: Option<PathBuf>,
    install: Option<bool>,
    program: Option<String>,
}

struct Settings {
    home: PathBuf,
    /// The harness's own folder.
    dir: PathBuf,
    /// Fleetlayer's folder for it (`files::state_dir`).
    state: PathBuf,
    program: String,
    install: bool,
}

struct Plugin {
    kind: Kind,
    settings: OnceLock<Settings>,
    /// What each shim session waits on (pi, OpenCode).
    prompts: Mutex<HashMap<String, BlockedPrompt>>,
    /// The sessions the hooks reported (Gemini CLI, Cursor).
    tracker: Mutex<hooks::Tracker>,
    /// A problem that stops it doing its job (the shim or hooks couldn't be installed, the hook socket couldn't open).
    problem: Mutex<Option<String>>,
}

impl Plugin {
    fn s(&self) -> Result<&Settings, RpcError> {
        self.settings.get().ok_or_else(|| RpcError::refused("initialize first"))
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>, RpcError> {
        let s = self.s()?;
        let harness = Some(self.kind.name().to_string());
        let mut out = vec![];
        if self.kind.uses_shim() {
            let prompts = self.prompts.lock().expect("prompts");
            for reg in shim::registrations(&s.state) {
                for sess in reg.sessions {
                    let meta = SessionMeta {
                        harness: harness.clone(),
                        address: reg.address.clone(),
                        deliverable: Some(true),
                        prompt: prompts.get(&sess.id).cloned().map(Some),
                        ..Default::default()
                    };
                    out.push(SessionInfo {
                        session_id: sess.id,
                        cwd: sess.cwd,
                        title: sess.title,
                        updated_at: None,
                        meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
                    });
                }
            }
        } else {
            let mut t = self.tracker.lock().expect("tracker");
            t.prune();
            for (id, sess) in &t.sessions {
                let meta = SessionMeta {
                    harness: harness.clone(),
                    address: sess.address.clone(),
                    deliverable: Some(false),
                    prompt: sess.prompt.clone().map(Some),
                    ..Default::default()
                };
                out.push(SessionInfo {
                    session_id: id.clone(),
                    cwd: sess.cwd.clone(),
                    title: None,
                    updated_at: None,
                    meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
                });
            }
        }
        Ok(out)
    }

    async fn deliver(&self, d: &Deliver) -> Result<Delivered, RpcError> {
        let s = self.s()?;
        if !self.kind.uses_shim() {
            return Err(RpcError::new(
                code::NO_CHANNEL,
                format!("{} takes no message from outside a running session", self.kind.title()),
            ));
        }
        let regs = shim::registrations(&s.state);
        let reg =
            shim::find(&regs, &d.session_id, Some(d.address.as_str()).filter(|a| !a.is_empty())).ok_or_else(|| {
                RpcError::new(
                    code::NO_CHANNEL,
                    format!("no {} process has session {}", self.kind.title(), d.session_id),
                )
            })?;
        match shim::deliver(reg, &d.session_id, &d.text).await {
            Ok(()) => Ok(Delivered { via: Via::HarnessChannel }),
            Err(shim::DeliverError::Gone(m)) => Err(RpcError::not_here(m)),
            Err(shim::DeliverError::NoChannel(m)) => Err(RpcError::new(code::NO_CHANNEL, m)),
            Err(shim::DeliverError::Failed(m)) => Err(RpcError::refused(m)),
        }
    }

    /// The harness's own transcript file for a session, if it keeps one (OpenCode doesn't).
    fn source(&self, id: &str) -> Option<PathBuf> {
        let s = self.settings.get()?;
        match self.kind {
            Kind::Pi => shim::registrations(&s.state)
                .into_iter()
                .flat_map(|r| r.sessions)
                .find(|x| x.id == id)
                .and_then(|x| x.file.map(PathBuf::from))
                .filter(|p| p.is_file())
                .or_else(|| {
                    let suffix = format!("_{id}.jsonl");
                    subfiles(&s.dir.join("sessions"), 1).into_iter().find(|p| file_name(p).ends_with(&suffix))
                }),
            Kind::Gemini => self.tracked_transcript(id).or_else(|| {
                let short = id.get(..8)?;
                subfiles(&s.dir.join("tmp"), 2).into_iter().find(|p| {
                    p.parent().and_then(Path::file_name).is_some_and(|n| n == "chats")
                        && file_name(p).ends_with(&format!("-{short}.jsonl"))
                        && gemini_session_id(p).as_deref() == Some(id)
                })
            }),
            Kind::Cursor => self.tracked_transcript(id).or_else(|| {
                subfiles(&s.dir.join("projects"), 3).into_iter().find(|p| file_name(p) == format!("{id}.jsonl"))
            }),
            Kind::Opencode => None,
        }
    }

    fn tracked_transcript(&self, id: &str) -> Option<PathBuf> {
        let t = self.tracker.lock().expect("tracker");
        t.sessions.get(id)?.transcript.as_ref().map(PathBuf::from).filter(|p| p.is_file())
    }

    /// The session's transcript in `fleetlayer-jsonl`, written (again) when the harness's copy is newer.
    fn transcript(&self, id: &str) -> Result<PathBuf, RpcError> {
        let s = self.s()?;
        check_id(id)?;
        let unknown = || RpcError::new(code::UNKNOWN_SESSION, format!("no {} session {id} here", self.kind.title()));
        let out = s.state.join("transcripts").join(format!("{id}.jsonl"));
        if self.kind == Kind::Opencode {
            let fresh = std::fs::metadata(&out)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < Duration::from_secs(5)));
            if !fresh {
                let export = opencode_export(&s.program, id).ok_or_else(unknown)?;
                transcript::write_lines(&out, &transcript::from_opencode_export(&export, id))
                    .map_err(RpcError::refused)?;
            }
            return Ok(out);
        }
        let source = self.source(id).ok_or_else(unknown)?;
        if transcript::stale(&source, &out) {
            let each = match self.kind {
                Kind::Pi => transcript::from_pi,
                Kind::Gemini => transcript::from_gemini,
                _ => transcript::from_cursor,
            };
            transcript::convert_jsonl(&source, &out, id, each).map_err(RpcError::refused)?;
        }
        Ok(out)
    }

    /// Every transcript it knows, newest first, each in `fleetlayer-jsonl`.
    fn transcripts(&self) -> Result<Vec<TranscriptFile>, RpcError> {
        let s = self.s()?;
        let ids: Vec<String> = match self.kind {
            Kind::Pi => newest(subfiles(&s.dir.join("sessions"), 1))
                .iter()
                .filter_map(|p| file_name(p).strip_suffix(".jsonl")?.rsplit_once('_').map(|(_, id)| id.to_string()))
                .collect(),
            Kind::Gemini => newest(subfiles(&s.dir.join("tmp"), 2))
                .iter()
                .filter(|p| p.parent().and_then(Path::file_name).is_some_and(|n| n == "chats"))
                .filter_map(|p| gemini_session_id(p))
                .collect(),
            Kind::Cursor => newest(subfiles(&s.dir.join("projects"), 3))
                .iter()
                .filter(|p| {
                    p.parent().and_then(Path::file_name).is_some_and(|n| *n == *file_name(p).trim_end_matches(".jsonl"))
                })
                .map(|p| file_name(p).trim_end_matches(".jsonl").to_string())
                .collect(),
            // OpenCode keeps no files of its own: the copies made so far, and the sessions running now.
            Kind::Opencode => {
                let mut ids: Vec<String> = shim::registrations(&s.state)
                    .into_iter()
                    .flat_map(|r| r.sessions.into_iter().map(|x| x.id))
                    .collect();
                ids.extend(
                    newest(subfiles(&s.state.join("transcripts"), 0))
                        .iter()
                        .filter_map(|p| file_name(p).strip_suffix(".jsonl").map(str::to_string)),
                );
                ids
            }
        };
        let running: HashSet<String> = if self.kind == Kind::Opencode {
            shim::registrations(&s.state).into_iter().flat_map(|r| r.sessions.into_iter().map(|x| x.id)).collect()
        } else {
            HashSet::new()
        };
        let mut seen = HashSet::new();
        let mut out = vec![];
        for id in ids.into_iter().filter(|i| check_id(i).is_ok() && seen.insert(i.clone())).take(TRANSCRIPTS_MAX) {
            // An OpenCode copy is exported again only for a session running now: an export takes most of a second.
            let path = if self.kind == Kind::Opencode && !running.contains(&id) {
                Some(s.state.join("transcripts").join(format!("{id}.jsonl"))).filter(|p| p.is_file()).ok_or(())
            } else {
                self.transcript(&id).map_err(drop)
            };
            if let Ok(path) = path {
                out.push(TranscriptFile {
                    session_id: id,
                    agent_id: None,
                    path: path.to_string_lossy().into_owned(),
                    format: transcript::FORMAT.into(),
                });
            }
        }
        Ok(out)
    }

    /// Where a stored session worked.
    fn locate(&self, id: &str) -> Option<String> {
        let s = self.settings.get()?;
        check_id(id).ok()?;
        match self.kind {
            Kind::Pi => first_value(&self.source(id)?, "cwd"),
            Kind::Opencode => opencode_export(&s.program, id)?["info"]["directory"].as_str().map(str::to_string),
            Kind::Gemini | Kind::Cursor => {
                if let Some(cwd) = self.tracker.lock().expect("tracker").sessions.get(id).and_then(|t| t.cwd.clone()) {
                    return Some(cwd);
                }
                // Gemini CLI writes the project's root beside its chats: tmp/<project>/.project_root.
                let src = self.source(id)?;
                let root = std::fs::read_to_string(src.parent()?.parent()?.join(".project_root")).ok()?;
                Some(root.trim().to_string()).filter(|r| r.starts_with('/'))
            }
        }
    }

    /// Get `cwd` ready for a session nobody is watching start: trust the folder where the harness asks.
    fn prepare(&self, cwd: &str) -> Result<(), String> {
        let s = self.settings.get().ok_or("initialize first")?;
        match self.kind {
            // pi asks before loading a project's own extensions and settings (`trust.json`, nearest folder wins).
            Kind::Pi => files::edit_json(&s.dir.join("trust.json"), |r| {
                r.insert(cwd.into(), Value::Bool(true));
                Ok(())
            })
            .map(drop),
            Kind::Gemini => files::edit_json(&s.dir.join("trustedFolders.json"), |r| {
                r.insert(cwd.into(), json!("TRUST_FOLDER"));
                Ok(())
            })
            .map(drop),
            // Cursor's CLI checks for `projects/<folder as a slug>/.workspace-trusted`.
            Kind::Cursor => {
                let marker = s.dir.join("projects").join(cursor_slug(cwd)).join(".workspace-trusted");
                if marker.exists() {
                    return Ok(());
                }
                let body = json!({"trustedAt": jiff::Timestamp::now().to_string(), "workspacePath": cwd,
                    "trustMethod": "cli-flag"});
                files::write_atomic(&marker, body.to_string().as_bytes())
            }
            Kind::Opencode => Ok(()),
        }
    }

    /// Put the shim or the hooks in place (or take them out), where the harness is installed.
    fn install(&self, exe: Option<&Path>) -> Result<bool, String> {
        let s = self.settings.get().ok_or("initialize first")?;
        if !s.dir.is_dir() {
            return Ok(false); // the harness isn't set up for this user (yet)
        }
        match self.kind {
            Kind::Pi | Kind::Opencode => {
                let (file, body) = match self.kind {
                    Kind::Pi => (s.dir.join("extensions/fleetlayer.ts"), PI_SHIM),
                    _ => (s.dir.join("plugins/fleetlayer.js"), OPENCODE_SHIM),
                };
                if s.install { files::ensure_file(&file, body) } else { files::remove(&file) }
            }
            Kind::Gemini | Kind::Cursor => {
                let command = match (s.install, exe) {
                    (false, _) => None,
                    (true, Some(exe)) => Some(hooks::command(exe, &s.home, self.kind)),
                    (true, None) => return Err("can't tell where fleetlayer-harness is, so no hooks".into()),
                };
                if self.kind == Kind::Gemini {
                    hooks::edit_gemini(&s.dir.join("settings.json"), command.as_deref())
                } else {
                    hooks::edit_cursor(&s.dir.join("hooks.json"), command.as_deref())
                }
            }
        }
    }
}

/// Session IDs name files here, so only plain ones: letters, digits, `-` and `_`.
fn check_id(id: &str) -> Result<(), RpcError> {
    if (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!("{id:?} isn't a session ID")))
    }
}

fn file_name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// The `.jsonl` files `depth` folders below `dir`.
fn subfiles(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten() {
        let p = e.path();
        if depth == 0 {
            if p.extension().is_some_and(|x| x == "jsonl") && p.is_file() {
                out.push(p);
            }
        } else if p.is_dir() {
            out.extend(subfiles(&p, depth - 1));
        }
    }
    out
}

fn newest(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    files.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    files
}

/// A string field of a JSONL file's first line.
fn first_value(path: &Path, key: &str) -> Option<String> {
    use std::io::BufRead;
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(path).ok()?).read_line(&mut first).ok()?;
    serde_json::from_str::<Value>(&first).ok()?.get(key)?.as_str().map(str::to_string)
}

fn gemini_session_id(chat: &Path) -> Option<String> {
    first_value(chat, "sessionId")
}

/// Cursor's folder name for a project: the path with each run of other characters as one `-`, trimmed.
fn cursor_slug(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// `opencode export <id>`, run for a moment with no input.
fn opencode_export(program: &str, id: &str) -> Option<Value> {
    let mut child = std::process::Command::new(program)
        .args(["export", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(20) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = reader.join().ok()?;
    // Some versions print a line or two before the JSON.
    let start = out.iter().position(|b| *b == b'{')?;
    serde_json::from_slice::<Value>(&out[start..]).ok().filter(|v| v["info"]["id"] == id)
}

/// A shim's prompt as the protocol has it.
fn to_prompt(v: &Value, at: jiff::Timestamp) -> Option<BlockedPrompt> {
    let id = v["id"].as_str()?.to_string();
    let kind = match v["kind"].as_str()? {
        "permission" => PromptKind::Permission,
        "plan" => PromptKind::Plan,
        _ => PromptKind::Question,
    };
    let questions = v["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| PromptQuestion {
            header: q["header"].as_str().unwrap_or("").into(),
            question: q["question"].as_str().unwrap_or("").into(),
            multi_select: q["multiSelect"].as_bool().unwrap_or(false),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    Some(PromptOption {
                        n: u32::try_from(o["n"].as_u64()?).ok()?,
                        label: o["label"].as_str()?.into(),
                        description: o["description"].as_str().map(str::to_string),
                    })
                })
                .collect(),
            other: None,
        })
        .collect();
    let tool = v
        .get("tool")
        .filter(|t| t.is_object())
        .map(|t| PromptTool { name: t["name"].as_str().unwrap_or("tool").into(), input: capped(&t["input"]) });
    Some(BlockedPrompt {
        id,
        kind,
        at,
        source: "hook".into(),
        questions,
        tool,
        plan: v["plan"].as_str().map(str::to_string),
        options: vec![],
        options_from: None,
    })
}

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

fn prompt_update(peer: &Peer, session: &str, prompt: Option<BlockedPrompt>) {
    let meta = SessionMeta { prompt: Some(prompt), ..Default::default() };
    let n = SessionNotification {
        session_id: session.to_string(),
        update: SessionUpdate::Info(SessionInfoUpdate {
            session_update: SessionInfoTag::SessionInfoUpdate,
            title: None,
            updated_at: None,
            meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
        }),
    };
    peer.notify(acp::method::SESSION_UPDATE, &n);
}

fn report_status(plugin: &Plugin, peer: &Peer) {
    let problem = plugin.problem.lock().expect("problem").clone();
    let sessions = plugin.sessions().map(|s| s.len()).unwrap_or(0);
    peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail: json!({"sessions": sessions}) });
}

/// Keep the shim or hooks installed (something may rewrite the harness's settings), every five minutes.
async fn keep_installed(plugin: Arc<Plugin>, peer: Peer) {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    while !peer.is_closed() {
        let p = plugin.clone();
        let exe = exe.clone();
        let result = tokio::task::spawn_blocking(move || p.install(exe.as_deref())).await.unwrap_or(Ok(false));
        match &result {
            Ok(true) => tracing::info!("{}: installed (or took out) Fleetlayer's shim or hooks", plugin.kind.name()),
            Ok(false) => {}
            Err(e) => tracing::warn!("{}: {e}", plugin.kind.name()),
        }
        *plugin.problem.lock().expect("problem") = result.err();
        report_status(&plugin, &peer);
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

/// Follow the shims: watch each agent's socket for what its sessions wait on, and say when the set of sessions changes.
async fn follow_shims(plugin: Arc<Plugin>, peer: Peer) {
    let watching: Arc<Mutex<HashSet<u32>>> = Arc::default();
    let mut last: Option<BTreeSet<String>> = None;
    while !peer.is_closed() {
        let Ok(s) = plugin.s() else { return };
        let regs = shim::registrations(&s.state);
        let now: BTreeSet<String> = regs.iter().flat_map(|r| r.sessions.iter().map(|x| x.id.clone())).collect();
        if last.as_ref().is_some_and(|l| *l != now) {
            peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
        }
        last = Some(now);
        for reg in regs {
            if !watching.lock().expect("watching").insert(reg.pid) {
                continue;
            }
            let (plugin, peer, watching) = (plugin.clone(), peer.clone(), watching.clone());
            tokio::spawn(async move {
                let mut mine: HashSet<String> = HashSet::new();
                let _ = shim::watch(&reg.socket, |states| {
                    let now = jiff::Timestamp::now();
                    let mut prompts = plugin.prompts.lock().expect("prompts");
                    for (session, p) in states {
                        mine.insert(session.clone());
                        let old = prompts.get(&session).cloned();
                        let new = p.and_then(|p| {
                            // The same prompt keeps the moment it appeared.
                            let at =
                                old.as_ref().filter(|o| Some(o.id.as_str()) == p["id"].as_str()).map_or(now, |o| o.at);
                            to_prompt(&p, at)
                        });
                        if new != old {
                            match &new {
                                Some(n) => prompts.insert(session.clone(), n.clone()),
                                None => prompts.remove(&session),
                            };
                            prompt_update(&peer, &session, new);
                        }
                    }
                })
                .await;
                let mut prompts = plugin.prompts.lock().expect("prompts");
                for s in mine {
                    prompts.remove(&s);
                }
                watching.lock().expect("watching").remove(&reg.pid);
            });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Listen for the hooks, and say when sessions start, end or change what they wait on.
async fn follow_hooks(plugin: Arc<Plugin>, peer: Peer) {
    let Ok(s) = plugin.s() else { return };
    let path = hooks::socket_path(&s.state);
    let saved = s.state.join("sessions.json");
    if let Err(e) = files::make_private_dir(&s.state) {
        *plugin.problem.lock().expect("problem") = Some(e);
        return;
    }
    // A socket nobody answers on is left over from an earlier run.
    if path.exists() && std::os::unix::net::UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            let m = format!("can't listen for {}'s hooks on {}: {e}", plugin.kind.title(), path.display());
            tracing::warn!("{m}");
            *plugin.problem.lock().expect("problem") = Some(m);
            report_status(&plugin, &peer);
            return;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    {
        let (plugin, peer) = (plugin.clone(), peer.clone());
        let saved = saved.clone();
        tokio::spawn(async move {
            while !peer.is_closed() {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let mut t = plugin.tracker.lock().expect("tracker");
                if t.prune() {
                    t.save(&saved);
                    peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
                }
            }
        });
    }
    while let Ok((stream, _)) = listener.accept().await {
        let (plugin, peer, saved) = (plugin.clone(), peer.clone(), saved.clone());
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt};
            let mut line = String::new();
            let mut r = tokio::io::BufReader::new(stream).take(4 << 20);
            if r.read_line(&mut line).await.is_err() {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { return };
            let mut t = plugin.tracker.lock().expect("tracker");
            match t.event(plugin.kind, &v, jiff::Timestamp::now()) {
                hooks::Change::None => {}
                hooks::Change::Prompt(session) => {
                    let p = t.sessions.get(&session).and_then(|x| x.prompt.clone());
                    prompt_update(&peer, &session, p);
                }
                hooks::Change::Sessions => {
                    t.save(&saved);
                    peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
                }
            }
        });
    }
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let plugin = &self.0;
        let kind = plugin.kind;
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-harness runs under a Fleetlayer node"))?;
                let config: Config = serde_json::from_value(node.config).unwrap_or_default();
                let home = PathBuf::from(&node.home);
                let settings = Settings {
                    dir: config
                        .dir
                        .map(|d| match d.strip_prefix("~") {
                            Ok(rest) => home.join(rest),
                            Err(_) => d,
                        })
                        .unwrap_or_else(|| kind.default_dir(&home)),
                    state: files::state_dir(&home, kind.name()),
                    program: config.program.unwrap_or_else(|| "opencode".into()),
                    install: config.install.unwrap_or(true),
                    home,
                };
                if plugin.settings.set(settings).is_ok() {
                    if kind == Kind::Cursor || kind == Kind::Gemini {
                        let s = plugin.s()?;
                        *plugin.tracker.lock().expect("tracker") = hooks::Tracker::load(&s.state.join("sessions.json"));
                        tokio::spawn(follow_hooks(plugin.clone(), peer.clone()));
                    } else {
                        let _ = files::make_private_dir(&plugin.s()?.state);
                        tokio::spawn(follow_shims(plugin.clone(), peer.clone()));
                    }
                    tokio::spawn(keep_installed(plugin.clone(), peer.clone()));
                }
                let meta = PluginMeta {
                    protocol: fleetlayer_protocol::PROTOCOL,
                    roles: Roles {
                        harness: Some(HarnessRole {
                            harnesses: vec![kind.name().into()],
                            deliver: true,
                            transcripts: true,
                            prepare: true,
                            locate: true,
                            list: true,
                        }),
                        ..Default::default()
                    },
                };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: format!("fleetlayer-harness-{}", kind.name()),
                        title: Some(kind.title().into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            acp::method::SESSION_LIST => {
                answer(&ListSessionsResponse { sessions: plugin.sessions()?, next_cursor: None })
            }
            ext::method::SESSION_DELIVER => {
                let d: Deliver = params(p)?;
                answer(&plugin.deliver(&d).await?)
            }
            ext::method::HARNESS_TRANSCRIPT => {
                let id =
                    p.get("sessionId").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("sessionId"))?;
                if p.get("agentId").is_some_and(|a| !a.is_null()) {
                    return Err(RpcError::new(
                        code::UNKNOWN_SESSION,
                        format!("{} has no subagent transcripts", kind.title()),
                    ));
                }
                let (pl, id) = (plugin.clone(), id.to_string());
                let path = tokio::task::spawn_blocking(move || pl.transcript(&id))
                    .await
                    .map_err(|e| RpcError::internal(e.to_string()))??;
                answer(&json!({"path": path, "format": transcript::FORMAT}))
            }
            ext::method::HARNESS_TRANSCRIPTS => {
                let pl = plugin.clone();
                let list = tokio::task::spawn_blocking(move || pl.transcripts())
                    .await
                    .map_err(|e| RpcError::internal(e.to_string()))??;
                answer(&json!({"transcripts": list}))
            }
            ext::method::HARNESS_PREPARE => {
                let r: ext::HarnessPrepare = params(p)?;
                if r.harness != kind.name() {
                    return Ok(json!({}));
                }
                plugin.prepare(&r.cwd).map_err(RpcError::refused)?;
                tracing::info!(cwd = %r.cwd, "got the folder ready for {}", kind.title());
                Ok(json!({}))
            }
            ext::method::HARNESS_LOCATE => {
                let id =
                    p.get("sessionId").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("sessionId"))?;
                let (pl, idc) = (plugin.clone(), id.to_string());
                let cwd = tokio::task::spawn_blocking(move || pl.locate(&idc)).await.ok().flatten();
                let cwd = cwd.ok_or_else(|| {
                    RpcError::new(code::UNKNOWN_SESSION, format!("no {} session {id} here", kind.title()))
                })?;
                answer(&ext::Located { cwd, harness: kind.name().into() })
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: fleetlayer-harness <pi|opencode|gemini|cursor>   (a Fleetlayer node starts it as a plugin)");
    eprintln!("       fleetlayer-harness hook <gemini|cursor>        (the harness runs it as a hook)");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("FLEETLAYER_LOG"))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["hook", h] => {
            if let Some(kind) = Kind::parse(h) {
                hooks::run_hook(kind);
            }
        }
        [h] => {
            let Some(kind) = Kind::parse(h) else { usage() };
            let plugin = Plugin {
                kind,
                settings: OnceLock::new(),
                prompts: Mutex::default(),
                tracker: Mutex::default(),
                problem: Mutex::default(),
            };
            fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(plugin)))).await;
        }
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_that_could_name_another_file_are_refused() {
        assert!(check_id("ses_dde9ed91affeWyvEuFUoS7fJMh").is_ok());
        assert!(check_id("01a22171-3f06-71e7-8170-6be23c6e3833").is_ok());
        for bad in ["", "../x", "a/b", "a b", ".."] {
            assert!(check_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cursor_slugs() {
        assert_eq!(cursor_slug("/Users/a/code/my.app"), "Users-a-code-my-app");
        assert_eq!(cursor_slug("/home/x//y_z/"), "home-x-y-z");
    }

    #[test]
    fn shim_prompts_become_protocol_prompts() {
        let at = jiff::Timestamp::now();
        let p = to_prompt(
            &json!({"id": "per_1", "kind": "permission", "tool": {"name": "bash", "input": {"command": "rm -rf x"}}}),
            at,
        )
        .unwrap();
        assert_eq!((p.kind, p.tool.unwrap().input["command"].as_str()), (PromptKind::Permission, Some("rm -rf x")));
        let q = to_prompt(
            &json!({"id": "q1", "kind": "question", "questions": [{"header": "H", "question": "Which?", "multiSelect": true,
                "options": [{"n": 1, "label": "A"}, {"n": 2, "label": "B", "description": "b"}]}]}),
            at,
        )
        .unwrap();
        assert_eq!(q.questions[0].options.len(), 2);
        assert!(q.questions[0].multi_select);
        assert!(to_prompt(&json!({"kind": "question"}), at).is_none(), "no id, no prompt");
    }

    fn plugin(kind: Kind, home: &Path) -> Plugin {
        let p = Plugin {
            kind,
            settings: OnceLock::new(),
            prompts: Mutex::default(),
            tracker: Mutex::default(),
            problem: Mutex::default(),
        };
        let _ = p.settings.set(Settings {
            home: home.into(),
            dir: home.join(match kind {
                Kind::Pi => ".pi/agent",
                Kind::Opencode => ".config/opencode",
                Kind::Gemini => ".gemini",
                Kind::Cursor => ".cursor",
            }),
            state: files::state_dir(home, kind.name()),
            program: "opencode-not-here".into(),
            install: true,
        });
        p
    }

    #[test]
    fn pi_transcripts_are_found_converted_and_located() {
        let home = tempfile::tempdir().unwrap();
        let p = plugin(Kind::Pi, home.path());
        let dir = home.path().join(".pi/agent/sessions/--w--");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-11-28T09-16-39-558Z_01a2-3f06.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"01a2-3f06","cwd":"/w"}"#, "\n",
                r#"{"type":"message","timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#, "\n"
            ),
        )
        .unwrap();
        let path = p.transcript("01a2-3f06").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""text":"hello""#) && text.contains(r#""type":"user""#), "{text}");
        assert_eq!(p.locate("01a2-3f06").as_deref(), Some("/w"));
        let list = p.transcripts().unwrap();
        assert_eq!((list.len(), list[0].format.as_str()), (1, "fleetlayer-jsonl"));
        assert!(p.transcript("nope").is_err());
    }

    #[test]
    fn folders_are_trusted_where_each_harness_asks() {
        let home = tempfile::tempdir().unwrap();
        plugin(Kind::Gemini, home.path()).prepare("/w/app").unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(home.path().join(".gemini/trustedFolders.json")).unwrap()).unwrap();
        assert_eq!(v["/w/app"], "TRUST_FOLDER");
        plugin(Kind::Pi, home.path()).prepare("/w/app").unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(home.path().join(".pi/agent/trust.json")).unwrap()).unwrap();
        assert_eq!(v["/w/app"], true);
        plugin(Kind::Cursor, home.path()).prepare("/w/app").unwrap();
        assert!(home.path().join(".cursor/projects/w-app/.workspace-trusted").is_file());
    }

    #[test]
    fn the_shim_goes_in_only_where_the_harness_is_set_up() {
        let home = tempfile::tempdir().unwrap();
        let p = plugin(Kind::Opencode, home.path());
        assert!(!p.install(None).unwrap(), "no ~/.config/opencode: nothing to do");
        std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        assert!(p.install(None).unwrap());
        assert_eq!(
            std::fs::read_to_string(home.path().join(".config/opencode/plugins/fleetlayer.js")).unwrap(),
            OPENCODE_SHIM
        );
        assert!(!p.install(None).unwrap(), "already there");
    }
}
