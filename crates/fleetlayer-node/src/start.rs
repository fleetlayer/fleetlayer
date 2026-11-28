//! Starting and resuming sessions, here or on another node (`fleetlayer session start|resume`, `docs/protocol.md`,
//! "Starting sessions"). The session manager plugin opens a tab without taking focus from anyone; the harness plugin
//! gets the folder ready first (Claude Code: marks it trusted, or the session would wait at the trust prompt). The
//! agent's arguments come from this node's own config (`harnesses:`) and Fleetlayer's few (`--name`, `--resume`),
//! never from the caller. The answer comes once the agent takes input and the session is listed, so a message sent
//! right after reaches it.

use std::time::Duration;

use fleetlayer_protocol::ext::{self, From, HarnessPrepare, Located, OpenSession, OpenedSession};
use fleetlayer_protocol::link::{self as l, SessionServe, SessionStart, SessionStarted, StartSpec};
use fleetlayer_protocol::rpc::{RpcError, code};
use serde_json::{Value, json};

use crate::Node;
use crate::sessions::Session;

/// How long a start may take on the node that runs it: herdr waits up to two minutes for the agent to take input,
/// then the session has to be listed.
pub const SERVE_TIMEOUT: Duration = Duration::from_secs(150);
/// A start on another node, through the control plane: a little more than the serving node's own limit.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(170);
/// How long to wait for a started session to show up in the node's list.
const LISTED_WITHIN: Duration = Duration::from_secs(15);

/// `node/session_open` from the CLI: start or resume here, or on `on` through the control plane.
pub async fn open(
    node: &Node,
    mut spec: StartSpec,
    on: Option<&str>,
    why: Option<String>,
    caller: Option<&Session>,
) -> Result<SessionStarted, RpcError> {
    let target = match on {
        Some(n) if n != node.name => Some(n.to_string()),
        Some(_) => None,
        // A resume by ID alone: here if this node knows the session, else wherever the fleet lists it.
        None => match &spec.resume {
            Some(id) if !knows(node, id).await => find_node(node, id).await?,
            _ => None,
        },
    };
    // A prompt from a session, or from another node, is a message from the caller: the agent sees who it's from and
    // where to answer. A person's own shell on this node types it as it is.
    if (caller.is_some() || target.is_some())
        && let Some(text) = spec.prompt.take()
    {
        let (from, reply_to) = sender(node, caller);
        spec.prompt = Some(crate::deliver::render_message(&from, &text, reply_to.as_deref()));
    }
    let Some(target) = target else { return here(node, spec).await };
    let peer = node.link.peer().await.ok_or_else(|| {
        RpcError::not_here(format!("{target} is another node, and {} isn't linked to a control plane now", node.name))
    })?;
    let req = SessionStart {
        node: target.clone(),
        spec,
        caller: caller.and_then(Session::tab).unwrap_or("-").to_string(),
        justification: why,
    };
    peer.request_within(l::method::SESSION_START, &req, REMOTE_TIMEOUT).await.map_err(|e| {
        if e.code == code::METHOD_NOT_FOUND {
            RpcError::refused(format!(
                "the control plane is older than this node and can't start sessions on other nodes: update it to \
                 Fleetlayer {} or later",
                env!("CARGO_PKG_VERSION")
            ))
        } else {
            e
        }
    })
}

/// Who a prompt from this node is from, and where its answer goes.
pub fn sender(node: &Node, caller: Option<&Session>) -> (From, Option<String>) {
    match caller {
        Some(c) => {
            let name = c.tab().unwrap_or(&c.session_id);
            let from = From {
                label: format!("{name} ({})", node.name),
                name: c.tab().map(str::to_string),
                node: Some(node.name.clone()),
                session_id: Some(c.session_id.clone()),
            };
            (from, c.tab().map(|t| format!("{t}@{}", node.name)))
        }
        None => (From { label: format!("a shell on {}", node.name), ..Default::default() }, None),
    }
}

/// Control plane → node: a start for a caller on another node, already allowed there.
pub async fn serve(node: &Node, req: SessionServe) -> Result<Value, RpcError> {
    tracing::info!(caller = %req.caller, tab = ?req.spec.tab, resume = ?req.spec.resume, "session start asked");
    let started = tokio::time::timeout(SERVE_TIMEOUT, here(node, req.spec))
        .await
        // Not a refusal: its tab may be open already, so the start still counts against the caller's limits.
        .map_err(|_| RpcError::internal("the session didn't start within two and a half minutes"))??;
    tracing::info!(caller = %req.caller, tab = %started.tab, address = %started.address, "session started");
    serde_json::to_value(started).map_err(|e| RpcError::internal(e.to_string()))
}

/// Whether this node can resume `id` itself.
async fn knows(node: &Node, id: &str) -> bool {
    node.registry.get(id).await.is_some() || locate(node, id).await.is_ok()
}

/// The node the fleet lists session `id` on.
async fn find_node(node: &Node, id: &str) -> Result<Option<String>, RpcError> {
    let unknown = || {
        RpcError::new(
            code::UNKNOWN_SESSION,
            format!("{} doesn't know session {id}, and no other node lists it: say where with --on", node.name),
        )
    };
    let Some(peer) = node.link.peer().await else { return Err(unknown()) };
    let fleet: Value = peer.request(l::method::FLEET, &json!({})).await?;
    let found = fleet["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["sessionId"].as_str() == Some(id))
        .and_then(|s| s["node"].as_str())
        .map(str::to_string);
    if found.is_some() {
        return Ok(found);
    }
    // Long gone from every list: the summaries remember where it ran.
    let hits: Value = peer.request(l::method::SUMMARIES_SEARCH, &json!({"session": id, "limit": 1})).await?;
    let list = hits["summaries"].as_array().or(hits.as_array()).cloned().unwrap_or_default();
    match list.iter().find_map(|h| h["node"].as_str()) {
        Some(n) => Ok(Some(n.to_string())),
        None => Err(unknown()),
    }
}

/// Where a stored session worked, from the harness plugins that can say.
async fn locate(node: &Node, id: &str) -> Result<Located, RpcError> {
    for p in &node.plugins {
        if !p.roles().await.harness.is_some_and(|h| h.locate) {
            continue;
        }
        let Some(peer) = p.peer().await else { continue };
        if let Ok(found) =
            peer.request_within::<_, Located>(ext::method::HARNESS_LOCATE, &json!({"sessionId": id}), secs(10)).await
        {
            return Ok(found);
        }
    }
    Err(RpcError::new(code::UNKNOWN_SESSION, format!("{} doesn't know session {id}", node.name)))
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Start or resume a session on this node.
pub async fn here(node: &Node, spec: StartSpec) -> Result<SessionStarted, RpcError> {
    let plan = plan(node, &spec).await?;
    let manager = manager(node).await?;
    let peer = manager.peer().await.ok_or_else(|| RpcError::refused(format!("{} isn't running", manager.name())))?;
    // Trusting the folder comes last before opening (the agent reads it as it starts), and is taken back if the
    // session doesn't open.
    let trusted = prepare(node, &plan.harness, &plan.cwd).await?;
    let open = OpenSession {
        label: plan.tab.clone(),
        cwd: plan.cwd.clone(),
        harness: plan.harness.clone(),
        args: plan.args.clone(),
        prompt: spec.prompt.clone(),
    };
    // Watch for the session before opening it, so its first listing isn't missed.
    let changes = node.registry.subscribe();
    let opened: OpenedSession = match peer.request_within(ext::method::SESSION_OPEN, &open, secs(140)).await {
        Ok(o) => o,
        Err(e) => {
            unprepare(node, &trusted, &plan.harness, &plan.cwd).await;
            return Err(if e.code == code::METHOD_NOT_FOUND {
                RpcError::refused(format!("{} can't open sessions", manager.name()))
            } else {
                e
            });
        }
    };
    let session_id =
        listed(node, &opened, changes).await.or(opened.session_id.clone()).filter(|id| !id.starts_with('@'));
    if let Some(e) = &opened.prompt_error {
        tracing::warn!(tab = %plan.tab, "the session started but its first prompt didn't go in: {e}");
    }
    Ok(SessionStarted {
        node: node.name.clone(),
        tab: plan.tab,
        address: opened.address,
        session_id,
        prompt_error: opened.prompt_error,
    })
}

/// What will run: the tab, folder, harness and the agent's arguments.
#[derive(Debug, PartialEq)]
struct Plan {
    tab: String,
    cwd: String,
    harness: String,
    args: Vec<String>,
}

async fn plan(node: &Node, spec: &StartSpec) -> Result<Plan, RpcError> {
    let running = node.registry.all().await.into_iter().filter(Session::running).collect::<Vec<_>>();
    let (tab, cwd, harness) = match &spec.resume {
        None => {
            let tab = spec.tab.clone().ok_or_else(|| RpcError::invalid_params("name the tab with --tab"))?;
            let cwd = spec.cwd.clone().ok_or_else(|| RpcError::invalid_params("say where with --cwd"))?;
            (tab, cwd, spec.harness.clone().unwrap_or_else(|| "claude".into()))
        }
        Some(id) => {
            if let Some(s) = running.iter().find(|s| &s.session_id == id) {
                return Err(RpcError::refused(format!(
                    "that session is already running, in a tab named {}",
                    s.tab().unwrap_or("(no name)")
                )));
            }
            let known = node.registry.get(id).await;
            let located = match known.as_ref().and_then(|s| s.cwd.clone()) {
                Some(cwd) => Some(Located {
                    cwd,
                    harness: known.as_ref().and_then(|s| s.meta.harness.clone()).unwrap_or_else(|| "claude".into()),
                }),
                None => locate(node, id).await.ok(),
            };
            let cwd = spec.cwd.clone().or(located.as_ref().map(|l| l.cwd.clone())).ok_or_else(|| {
                RpcError::new(code::UNKNOWN_SESSION, format!("{} doesn't know where session {id} worked", node.name))
            })?;
            let harness = spec.harness.clone().or(located.map(|l| l.harness)).unwrap_or_else(|| "claude".into());
            let tab = spec
                .tab
                .clone()
                .or(known.as_ref().and_then(|s| s.tab().map(str::to_string)))
                .unwrap_or_else(|| format!("resumed {}", id.chars().take(8).collect::<String>()));
            (tab, cwd, harness)
        }
    };
    check_tab(&tab)?;
    if running.iter().any(|s| s.tab() == Some(&tab)) {
        return Err(RpcError::refused(format!("a session is already running in a tab named {tab} on {}", node.name)));
    }
    let cwd = check_dir(&cwd, &node.home)?;
    let own = node.harnesses.get(&harness).map(|h| h.args.clone()).unwrap_or_default();
    let mut args = agent_args(&harness, &own, &tab, spec.resume.as_deref())?;
    args.splice(own.len()..own.len(), model_args(&harness, spec.model.as_deref(), spec.effort.as_deref())?);
    Ok(Plan { tab, cwd, harness, args })
}

fn check_tab(tab: &str) -> Result<(), RpcError> {
    if tab.trim().is_empty() || tab.contains('@') || tab.chars().any(char::is_control) || tab.chars().count() > 64 {
        return Err(RpcError::invalid_params(format!(
            "{tab:?} can't be a tab's name: up to 64 characters, no '@' or control characters"
        )));
    }
    Ok(())
}

/// The folder, absolute and existing, and never the home folder (Claude Code asks about trusting it every time). A
/// leading `~/` is this node's home: a caller on another node can't know it.
fn check_dir(cwd: &str, home: &std::path::Path) -> Result<String, RpcError> {
    let expanded = match cwd.strip_prefix('~') {
        Some("") => home.to_string_lossy().into_owned(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home.display()),
        _ => cwd.to_string(),
    };
    let cwd = expanded.as_str();
    if !cwd.starts_with('/') {
        return Err(RpcError::invalid_params(format!("{cwd} isn't an absolute folder")));
    }
    let real = std::fs::canonicalize(cwd).map_err(|_| RpcError::refused(format!("there's no folder {cwd}")))?;
    if !real.is_dir() {
        return Err(RpcError::refused(format!("{cwd} isn't a folder")));
    }
    if std::fs::canonicalize(home).is_ok_and(|h| h == real) {
        return Err(RpcError::refused(
            "sessions don't start in the home folder (Claude Code asks whether to trust it every time): use a \
             project or scratch folder",
        ));
    }
    Ok(real.to_string_lossy().into_owned())
}

/// The agent's arguments: this node's own for the harness, then Fleetlayer's.
fn agent_args(harness: &str, own: &[String], tab: &str, resume: Option<&str>) -> Result<Vec<String>, RpcError> {
    let mut a = own.to_vec();
    match (harness, resume) {
        ("claude", None) => a.extend(["--name".into(), tab.into()]),
        ("claude", Some(id)) => a.extend(["--name".into(), tab.into(), "--resume".into(), id.into()]),
        ("codex", Some(id)) => a.extend(["resume".into(), id.into()]),
        // pi and oh-my-pi show the tab's name as the session's.
        ("pi", None) => a.extend(["--name".into(), tab.into()]),
        ("pi", Some(id)) => a.extend(["--name".into(), tab.into(), "--session".into(), id.into()]),
        ("omp", Some(id)) => a.push(format!("--resume={id}")),
        ("opencode", Some(id)) => a.extend(["--session".into(), id.into()]),
        ("cursor" | "gemini", Some(id)) => a.extend(["--resume".into(), id.into()]),
        (_, None) => {}
        (h, Some(_)) => return Err(RpcError::refused(format!("Fleetlayer doesn't know how to resume a {h} session"))),
    }
    // Letters, digits, `-` and `_` only: pi takes a path for `--session`, and an ID is never one.
    if let Some(id) = resume
        && (id.is_empty()
            || id.len() > 128
            || id.starts_with('-')
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    {
        return Err(RpcError::invalid_params(format!("{id:?} isn't a session ID")));
    }
    Ok(a)
}

/// The model and effort as the harness takes them: `claude --model X --effort Y`, `codex -m X -c
/// model_reasoning_effort=Y`. Placed after this node's own arguments and before Fleetlayer's (codex's `resume`
/// subcommand comes last).
fn model_args(harness: &str, model: Option<&str>, effort: Option<&str>) -> Result<Vec<String>, RpcError> {
    for (what, v) in [("model", model), ("effort", effort)] {
        if let Some(v) = v
            && (v.is_empty()
                || v.starts_with('-')
                || !v.chars().all(|c| c.is_ascii_alphanumeric() || "._-[]:/".contains(c)))
        {
            return Err(RpcError::invalid_params(format!("{v:?} isn't a {what}")));
        }
    }
    let mut a: Vec<String> = Vec::new();
    match harness {
        "claude" => {
            if let Some(m) = model {
                a.extend(["--model".into(), m.into()]);
            }
            if let Some(e) = effort {
                a.extend(["--effort".into(), e.into()]);
            }
        }
        "codex" => {
            if let Some(m) = model {
                a.extend(["-m".into(), m.into()]);
            }
            if let Some(e) = effort {
                a.extend(["-c".into(), format!("model_reasoning_effort={e}")]);
            }
        }
        // `--model X`, and pi's thinking level as the effort.
        "pi" | "omp" => {
            if let Some(m) = model {
                a.extend(["--model".into(), m.into()]);
            }
            if let Some(e) = effort {
                a.extend(["--thinking".into(), e.into()]);
            }
        }
        // A model and no effort setting: OpenCode's TUI takes `provider/model`, Cursor's effort is part of some
        // models' names, Gemini's thinking budget lives in its settings.
        "opencode" | "cursor" | "gemini" if effort.is_some() => {
            return Err(RpcError::refused(format!(
                "Fleetlayer can't choose a {harness} session's effort: pick a model that has it, or set it in \
                 {harness}'s own settings"
            )));
        }
        "opencode" | "gemini" => {
            if let Some(m) = model {
                a.extend(["-m".into(), m.into()]);
            }
        }
        "cursor" => {
            if let Some(m) = model {
                a.extend(["--model".into(), m.into()]);
            }
        }
        h if model.is_some() || effort.is_some() => {
            return Err(RpcError::refused(format!(
                "Fleetlayer doesn't know how to choose a {h} session's model or effort"
            )));
        }
        _ => {}
    }
    Ok(a)
}

/// Ask the harness plugins for this harness to get the folder ready. The answer names the plugins that added
/// something for it (`{"added": true}`), the only ones asked to take it back if the session doesn't open: a folder
/// already trusted (a session running there, the person's own trust) is never taken back.
async fn prepare(node: &Node, harness: &str, cwd: &str) -> Result<Vec<String>, RpcError> {
    let mut added = vec![];
    for p in &node.plugins {
        let Some(role) = p.roles().await.harness else { continue };
        if !role.prepare || !role.harnesses.iter().any(|h| h == harness) {
            continue;
        }
        let failed = |why: String| RpcError::refused(format!("getting {cwd} ready for {harness}: {why}"));
        let result = match p.peer().await {
            None => Err(RpcError::refused(format!("{} isn't running, so the folder can't be got ready", p.name()))),
            Some(peer) => {
                let req = HarnessPrepare { harness: harness.into(), cwd: cwd.into(), undo: false };
                peer.request_within::<_, Value>(ext::method::HARNESS_PREPARE, &req, secs(10))
                    .await
                    .map_err(|e| failed(e.message))
            }
        };
        match result {
            Ok(v) if v["added"] == true => added.push(p.name().to_string()),
            Ok(_) => {}
            Err(e) => {
                unprepare(node, &added, harness, cwd).await;
                return Err(e);
            }
        }
    }
    Ok(added)
}

/// Take back what `prepare` added for the folder, in the plugins that said they added something.
async fn unprepare(node: &Node, plugins: &[String], harness: &str, cwd: &str) {
    for name in plugins {
        let Some(peer) = (match node.plugin(name) {
            Some(p) => p.peer().await,
            None => None,
        }) else {
            continue;
        };
        let req = HarnessPrepare { harness: harness.into(), cwd: cwd.into(), undo: true };
        if let Err(e) = peer.request_within::<_, Value>(ext::method::HARNESS_PREPARE, &req, secs(10)).await {
            tracing::warn!("{name} couldn't take back the trust of {cwd} after a failed start: {}", e.message);
        }
    }
}

/// The session manager that opens sessions here.
async fn manager(node: &Node) -> Result<std::sync::Arc<crate::plugins::PluginHandle>, RpcError> {
    for p in &node.plugins {
        if p.roles().await.session_manager.is_some_and(|r| r.open) {
            return Ok(p.clone());
        }
    }
    Err(RpcError::refused(format!("no session manager on {} can open sessions", node.name)))
}

/// The started session's ID, once the node lists it at the address it opened at.
async fn listed(
    node: &Node,
    opened: &OpenedSession,
    mut changes: tokio::sync::broadcast::Receiver<crate::sessions::Change>,
) -> Option<String> {
    let find = || async {
        if let Some(id) = &opened.session_id
            && node.registry.get(id).await.is_some()
        {
            return Some(id.clone());
        }
        node.registry.by_address(&opened.address).await.map(|s| s.session_id)
    };
    let wait = async {
        loop {
            if let Some(id) = find().await {
                // An unnamed session gets its harness's ID soon after; give it a moment.
                if !id.starts_with('@') {
                    return Some(id);
                }
            }
            if let Err(tokio::sync::broadcast::error::RecvError::Closed) = changes.recv().await {
                return None;
            }
        }
    };
    match tokio::time::timeout(LISTED_WITHIN, wait).await {
        Ok(id) => id,
        Err(_) => find().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_come_from_the_node_then_fleetlayer() {
        let own = vec!["--permission-mode".to_string(), "acceptEdits".to_string()];
        assert_eq!(agent_args("claude", &own, "t", None).unwrap().join(" "), "--permission-mode acceptEdits --name t");
        assert_eq!(agent_args("claude", &[], "t", Some("s1")).unwrap().join(" "), "--name t --resume s1");
        assert_eq!(agent_args("codex", &["--x".into()], "t", Some("s1")).unwrap().join(" "), "--x resume s1");
        assert!(agent_args("codex", &[], "t", None).unwrap().is_empty());
        assert!(agent_args("other", &[], "t", Some("s1")).is_err());
        assert_eq!(agent_args("pi", &[], "t", None).unwrap().join(" "), "--name t");
        assert_eq!(agent_args("pi", &[], "t", Some("s1")).unwrap().join(" "), "--name t --session s1");
        assert_eq!(agent_args("omp", &[], "t", Some("s1")).unwrap().join(" "), "--resume=s1");
        assert_eq!(agent_args("opencode", &[], "t", Some("ses_1")).unwrap().join(" "), "--session ses_1");
        assert_eq!(agent_args("cursor", &[], "t", Some("c1")).unwrap().join(" "), "--resume c1");
        assert_eq!(agent_args("gemini", &[], "t", Some("g1")).unwrap().join(" "), "--resume g1");
        assert!(agent_args("gemini", &[], "t", None).unwrap().is_empty());
        assert!(agent_args("omp", &[], "t", Some("--yolo")).is_err());
        assert!(agent_args("pi", &[], "t", Some("/home/a/.pi/agent/sessions/x/y.jsonl")).is_err(), "never a path");
        assert!(agent_args("pi", &[], "t", Some("../y")).is_err());
        // A session ID can't smuggle in a flag.
        assert!(agent_args("claude", &[], "t", Some("--dangerously-skip-permissions")).is_err());
        assert!(agent_args("claude", &[], "t", Some("a b")).is_err());
    }

    #[test]
    fn the_folder_is_real_and_never_home() {
        let home = tempfile::tempdir().unwrap();
        let work = home.path().join("work");
        std::fs::create_dir(&work).unwrap();
        assert_eq!(
            check_dir(work.to_str().unwrap(), home.path()).unwrap(),
            work.canonicalize().unwrap().to_str().unwrap()
        );
        let e = check_dir(home.path().to_str().unwrap(), home.path()).unwrap_err();
        assert!(e.message.contains("home folder"), "{e:?}");
        // The home folder by another path is still home.
        let again = format!("{}/work/..", home.path().display());
        assert!(check_dir(&again, home.path()).is_err());
        assert!(check_dir("relative", home.path()).unwrap_err().message.contains("absolute"));
        assert!(check_dir("/no/such/folder", home.path()).unwrap_err().message.contains("no folder"));
        // ~/ is this node's home (a caller elsewhere can't know it); ~ alone is home, still refused; ~user isn't ours.
        assert_eq!(check_dir("~/work", home.path()).unwrap(), work.canonicalize().unwrap().to_str().unwrap());
        assert!(check_dir("~", home.path()).unwrap_err().message.contains("home folder"));
        assert!(check_dir("~/", home.path()).unwrap_err().message.contains("home folder"));
        assert!(check_dir("~other/work", home.path()).unwrap_err().message.contains("absolute"));
    }

    #[test]
    fn model_and_effort_go_in_as_each_harness_takes_them() {
        assert_eq!(model_args("claude", Some("opus"), Some("high")).unwrap().join(" "), "--model opus --effort high");
        assert_eq!(model_args("claude", None, Some("max")).unwrap().join(" "), "--effort max");
        assert_eq!(
            model_args("codex", Some("gpt-6"), Some("xhigh")).unwrap().join(" "),
            "-m gpt-6 -c model_reasoning_effort=xhigh"
        );
        assert!(model_args("claude", None, None).unwrap().is_empty());
        assert!(model_args("other", None, None).unwrap().is_empty());
        assert!(model_args("other", Some("x"), None).is_err());
        // Neither can smuggle in a flag or a second setting.
        assert!(model_args("claude", Some("--dangerously-skip-permissions"), None).is_err());
        assert!(model_args("codex", None, Some("high -c sandbox_mode=danger-full-access")).is_err());
        assert!(model_args("codex", None, Some("high\nx")).is_err());
        assert!(model_args("claude", Some("claude-opus-5-5[1m]"), None).is_ok());
        assert_eq!(model_args("pi", Some("sonnet"), Some("high")).unwrap().join(" "), "--model sonnet --thinking high");
        assert_eq!(model_args("opencode", Some("anthropic/x"), None).unwrap().join(" "), "-m anthropic/x");
        assert_eq!(model_args("gemini", Some("gemini-3-pro"), None).unwrap().join(" "), "-m gemini-3-pro");
        assert_eq!(model_args("cursor", Some("gpt-6"), None).unwrap().join(" "), "--model gpt-6");
        assert!(model_args("cursor", None, Some("high")).is_err());
        assert!(model_args("gemini", Some("x"), Some("high")).is_err());
    }

    #[test]
    fn tab_names_are_plain() {
        assert!(check_tab("fleetlayer docs").is_ok());
        assert!(check_tab("a@b").is_err());
        assert!(check_tab(" ").is_err());
        assert!(check_tab("a\nb").is_err());
    }
}
