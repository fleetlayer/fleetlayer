//! Role windows on the node (`docs/access-control.md`, "The admin window"). The control plane tells a node when a
//! window of a role it's in opens or closes (`link/window`). The node keeps each open window on disk with its end, and
//! closes it at that time by itself, linked or not, and again after a restart: the end of a window doesn't depend on the
//! control plane being reachable.
//!
//! Closing a window, on one of the role's nodes: every session here is closed (the role is this node's Unix user, so
//! all of them are its), everything delivered to the node is removed, and the `on_end` hooks run (the documented use:
//! remove a sudoers drop-in). Then it confirms to the control plane (`link/window_closed`), with what went wrong if
//! anything did. Nodes that only watch the role confirm too, since their hook is often what removes the rights. A node
//! that only watches a role (`windows:` in its config, a trusted runner on another Unix user) runs
//! its hooks: `on_start` when a window opens (add the drop-in), `on_end` when it closes. Hooks must be safe to run
//! twice.
//!
//! ```yaml
//! windows:
//!   - role: admin
//!     on_start: [sudo, -n, /usr/local/sbin/admin-window, open]
//!     on_end: [sudo, -n, /usr/local/sbin/admin-window, close]
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::link::{self as l, SessionAction, WindowClosed, WindowNotice};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::Node;

/// How long a hook may run.
const HOOK_TIMEOUT: Duration = Duration::from_secs(120);

/// Hooks for one role's windows (`windows:` in node.yaml).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowHooks {
    pub role: String,
    /// Run when a window opens: a program and its arguments.
    #[serde(default)]
    pub on_start: Vec<String>,
    /// Run when a window closes (at its end, early, or found ended after a restart).
    #[serde(default)]
    pub on_end: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Open {
    role: String,
    ends_at: i64,
    member: bool,
}

#[derive(Default)]
pub struct Windows {
    pub hooks: Vec<WindowHooks>,
    /// Open windows by ID, as saved on disk.
    open: Mutex<HashMap<String, Open>>,
    /// Closed windows by ID, and what went wrong closing each (None: nothing), to confirm again when asked.
    closed: Mutex<HashMap<String, Option<String>>>,
    /// Timers that close each open window at its end.
    timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl Windows {
    pub fn new(hooks: Vec<WindowHooks>) -> Windows {
        Windows { hooks, ..Default::default() }
    }

    /// The roles this node runs hooks for, sent in `link/hello`.
    pub fn watched(&self) -> Vec<String> {
        let mut r: Vec<String> = self.hooks.iter().map(|h| h.role.clone()).collect();
        r.sort();
        r.dedup();
        r
    }

    /// Closed windows that went wrong, for the node's health: a window that didn't close cleanly is a problem until
    /// the next one closes cleanly.
    pub async fn problems(&self) -> Vec<String> {
        self.closed.lock().await.iter().filter_map(|(id, p)| p.as_ref().map(|p| format!("window {id}: {p}"))).collect()
    }
}

fn path(node: &Node) -> PathBuf {
    node.home.join(".local/state/fleetlayer/windows.json")
}

async fn save(node: &Node) {
    let open = node.windows.open.lock().await.clone();
    let p = path(node);
    let write = || -> std::io::Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&open).unwrap_or_default())?;
        std::fs::rename(&tmp, &p)
    };
    if let Err(e) = write() {
        tracing::warn!("saving the open windows to {}: {e}", p.display());
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// At start: windows saved as open that ended while the node wasn't running are closed now; the rest get their timers.
pub async fn at_start(node: Arc<Node>) {
    let saved: HashMap<String, Open> =
        std::fs::read(path(&node)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for (id, o) in saved {
        let n = WindowNotice { id, role: o.role, open: true, ends_at: o.ends_at, member: o.member };
        notice(node.clone(), n).await;
    }
}

/// `link/window` from the control plane (or a saved window at start).
pub async fn notice(node: Arc<Node>, n: WindowNotice) {
    if n.open && n.ends_at > now_ms() {
        let first = node
            .windows
            .open
            .lock()
            .await
            .insert(n.id.clone(), Open { role: n.role.clone(), ends_at: n.ends_at, member: n.member })
            .is_none();
        save(&node).await;
        arm(node.clone(), n.id.clone(), n.ends_at).await;
        if first && !node.windows.closed.lock().await.contains_key(&n.id) {
            tracing::info!(role = %n.role, id = %n.id, ends_at = n.ends_at, "a window opened");
            for h in node.windows.hooks.iter().filter(|h| h.role == n.role && !h.on_start.is_empty()) {
                if let Err(e) = hook(&h.on_start, &n).await {
                    tracing::error!(role = %n.role, "the window's on_start hook failed: {e}");
                }
            }
        }
        return;
    }
    close(&node, &n).await;
}

/// Close the window at its end.
async fn arm(node: Arc<Node>, id: String, ends_at: i64) {
    let wait = Duration::from_millis((ends_at - now_ms()).max(0) as u64);
    let n2 = node.clone();
    let id2 = id.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let open = n2.windows.open.lock().await.get(&id2).cloned();
        if let Some(o) = open {
            let n = WindowNotice { id: id2, role: o.role, open: false, ends_at: o.ends_at, member: o.member };
            close(&n2, &n).await;
        }
    });
    if let Some(old) = node.windows.timers.lock().await.insert(id, task) {
        old.abort();
    }
}

async fn close(node: &Arc<Node>, n: &WindowNotice) {
    let opened = node.windows.open.lock().await.remove(&n.id);
    // Already closed here (the control plane missed the confirmation): confirm again.
    let done = node.windows.closed.lock().await.get(&n.id).cloned();
    if let Some(problem) = done {
        confirm(node, &n.id, problem).await;
        return;
    }
    // The timer that called us is the one being taken out; aborting it would stop this very close.
    if let Some(t) = node.windows.timers.lock().await.remove(&n.id)
        && opened.is_none()
    {
        t.abort();
    }
    save(node).await;
    let member = n.member || opened.as_ref().is_some_and(|o| o.member);
    tracing::info!(role = %n.role, id = %n.id, member, "closing a window");
    let mut problems: Vec<String> = vec![];
    if member {
        problems.extend(close_sessions(node).await);
        match forget_delivered(node).await {
            Ok(n) if n > 0 => tracing::info!(items = n, "removed what was delivered to this node"),
            Ok(_) => {}
            Err(e) => problems.push(format!("removing what was delivered: {e}")),
        }
    }
    for h in node.windows.hooks.iter().filter(|h| h.role == n.role && !h.on_end.is_empty()) {
        if let Err(e) = hook(&h.on_end, n).await {
            problems.push(format!("the on_end hook: {e}"));
        }
    }
    let problem = (!problems.is_empty()).then(|| problems.join("; "));
    if let Some(p) = &problem {
        tracing::error!(role = %n.role, id = %n.id, "the window didn't close cleanly: {p}");
    }
    {
        let mut closed = node.windows.closed.lock().await;
        // A clean close clears the older problems: they're about windows that are over.
        if problem.is_none() {
            closed.retain(|_, p| p.is_none());
        }
        closed.insert(n.id.clone(), problem.clone());
    }
    // Every node the window concerns confirms, the ones that only watch it too: their hook is what takes the sudo rule
    // away.
    confirm(node, &n.id, problem).await;
}

async fn confirm(node: &Node, id: &str, problem: Option<String>) {
    // Not linked: the control plane tells the node again when it links, and it confirms then.
    if let Some(peer) = node.link.peer().await {
        let c = WindowClosed { id: id.to_string(), problem };
        if let Err(e) =
            peer.request_within::<_, serde_json::Value>(l::method::WINDOW_CLOSED, &c, Duration::from_secs(15)).await
        {
            tracing::warn!(id, "confirming a closed window: {}", e.message);
        }
    }
}

/// Close every session on this node; what couldn't be closed.
async fn close_sessions(node: &Node) -> Vec<String> {
    let mut problems = vec![];
    for s in node.registry.all().await {
        if s.manager.is_none() {
            // Nothing can close it from here (a session no session manager reports).
            problems.push(format!("session {} has no session manager to close it", s.tab().unwrap_or(&s.session_id)));
            continue;
        }
        if let Err(e) = crate::act::here(node, &s.session_id, SessionAction::Close, "the window's end").await {
            problems.push(format!("closing session {}: {}", s.tab().unwrap_or(&s.session_id), e.message));
        }
    }
    problems
}

/// Remove everything delivered to this node (secrets, files from sync bundles): `crate::sync::forget_all`, which
/// asks every sync target plugin to remove what it wrote. The node keeps redacting those values from what leaves it.
async fn forget_delivered(node: &Arc<Node>) -> anyhow::Result<usize> {
    crate::sync::forget_all(node).await
}

/// Run a hook with the window in its environment; an error says how it failed, with the end of what it printed.
async fn hook(argv: &[String], n: &WindowNotice) -> anyhow::Result<()> {
    let (prog, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("an empty hook"))?;
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .env("FLEETLAYER_WINDOW_ROLE", &n.role)
        .env("FLEETLAYER_WINDOW_ID", &n.id)
        .env("FLEETLAYER_WINDOW_ENDS_AT", n.ends_at.to_string())
        .env("FLEETLAYER_WINDOW_STATE", if n.open { "open" } else { "closed" })
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(HOOK_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("{prog} ran for more than {}s and was stopped", HOOK_TIMEOUT.as_secs()))?
        .map_err(|e| anyhow::anyhow!("{prog} couldn't start: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: String = err.trim().chars().rev().take(300).collect::<String>().chars().rev().collect();
    anyhow::bail!(
        "{prog} exited with {}{}",
        out.status,
        if tail.is_empty() { String::new() } else { format!(": {tail}") }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(open: bool) -> WindowNotice {
        WindowNotice { id: "ap_1".into(), role: "admin".into(), open, ends_at: 0, member: false }
    }

    #[tokio::test]
    async fn a_hook_gets_the_window_and_its_failure_says_why() {
        let dir = std::env::temp_dir().join(format!("fl-window-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let script = format!("echo \"$FLEETLAYER_WINDOW_ROLE $FLEETLAYER_WINDOW_STATE\" > {}", out.display());
        hook(&["sh".into(), "-c".into(), script], &notice(false)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "admin closed");
        let e = hook(&["sh".into(), "-c".into(), "echo no sudo rule >&2; exit 3".into()], &notice(true))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("exit status: 3") && e.contains("no sudo rule"), "{e}");
        assert!(
            hook(&["/nonexistent/hook".into()], &notice(true))
                .await
                .unwrap_err()
                .to_string()
                .contains("couldn't start")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
