//! The merged herdr socket: herdr's own API (NDJSON `{id, method, params}`), answering for every herdr in the fleet.
//!
//! - Requests about this machine's panes go to its herdr unchanged, so everything herdr does keeps working.
//! - Lists (`agent.list`, `tab.list`, `pane.list`, `workspace.list`, `session.snapshot`) get the other machines'
//!   sessions added, as workspaces of their own (`fleet.rs`).
//! - Requests that name another machine's pane, tab or workspace are answered here: reads, prompts, keys and closing
//!   go through Fleetlayer (`term.rs`, the node), under the access policy. Anything else is `unsupported_method`, which
//!   herdr's client contract says clients handle as a normal error.
//! - `events.subscribe` streams this machine's herdr events and the other machines' changes on one connection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{RwLock, broadcast, mpsc};

use crate::fleet::{Found, Model, Pane, status_event, subscription_type};
use crate::term::{Failure, Pool};

/// Closes another machine's session, through the node (grant `session.close`).
#[async_trait::async_trait]
pub trait Closer: Send + Sync {
    async fn close(&self, address: &str) -> Result<(), Failure>;
}

pub struct Ctx {
    pub model: Arc<RwLock<Model>>,
    pub events: broadcast::Sender<Value>,
    /// This machine's herdr socket.
    pub local_socket: PathBuf,
    pub pool: Arc<Pool>,
    pub closer: Arc<dyn Closer>,
    /// The node's tools socket, for the HERDR_BIN shim's terminal streams.
    pub tools_socket: PathBuf,
}

/// The private method the HERDR_BIN shim asks: is this target another machine's, and what's its address?
pub const RESOLVE: &str = "_fleetlayer.dev/herdr/resolve";

/// A line-at-a-time connection to this machine's herdr.
struct Upstream {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl Upstream {
    async fn connect(path: &std::path::Path) -> std::io::Result<Upstream> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Upstream { lines: BufReader::new(r).lines(), w })
    }

    async fn call(&mut self, line: &str) -> std::io::Result<String> {
        self.w.write_all(line.as_bytes()).await?;
        self.w.write_all(b"\n").await?;
        self.lines.next_line().await?.ok_or_else(|| std::io::Error::other("herdr hung up"))
    }
}

fn error(id: &Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "error": {"code": code, "message": message.into()}})
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"id": id, "result": result})
}

/// Serve one client connection.
pub async fn connection(ctx: Arc<Ctx>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let mut upstream: Option<Upstream> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let e = error(&Value::Null, "invalid_request", "not valid JSON");
                if w.write_all(format!("{e}\n").as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
        };
        if req["method"] == "events.subscribe" {
            // A subscription connection carries nothing else (herdr's client contract).
            subscribe(ctx, req, w).await;
            return;
        }
        let answer = route(&ctx, &mut upstream, &line, &req).await;
        if w.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Send a request to this machine's herdr as it came, and answer its reply.
async fn forward(ctx: &Ctx, upstream: &mut Option<Upstream>, line: &str) -> Result<Value, String> {
    for attempt in 0..2 {
        if upstream.is_none() {
            match Upstream::connect(&ctx.local_socket).await {
                Ok(u) => *upstream = Some(u),
                Err(e) => return Err(format!("herdr isn't running on this machine ({e})")),
            }
        }
        let u = upstream.as_mut().expect("connected above");
        match u.call(line).await {
            Ok(reply) => return serde_json::from_str(&reply).map_err(|e| format!("herdr's reply: {e}")),
            // herdr restarted under us: connect once more.
            Err(e) if attempt == 0 => {
                tracing::debug!("herdr connection lost ({e}); reconnecting");
                *upstream = None;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    unreachable!("the loop returns")
}

/// The remote pane or workspace a request names, by any of herdr's target fields.
fn remote_target(model: &Model, params: &Value) -> Option<Found> {
    for key in ["target", "pane_id", "tab_id", "workspace_id", "terminal_id"] {
        if let Some(id) = params.get(key).and_then(Value::as_str)
            && let Some(f) = model.find(id)
        {
            return Some(f);
        }
    }
    None
}

/// A workspace filter that names another machine's workspace: `Some(Some(n))`; one of this machine's: `Some(None)`.
fn workspace_filter(model: &Model, params: &Value) -> Option<Option<u32>> {
    let id = params.get("workspace_id").and_then(Value::as_str)?;
    match model.find(id) {
        Some(Found::Workspace(n)) => Some(Some(n)),
        _ => Some(None),
    }
}

async fn route(ctx: &Ctx, upstream: &mut Option<Upstream>, line: &str, req: &Value) -> Value {
    let id = &req["id"];
    let method = req["method"].as_str().unwrap_or_default();
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let model = ctx.model.read().await;
    match method {
        RESOLVE => {
            let target = params["target"].as_str().unwrap_or_default();
            let found = remote_target(&model, &json!({"target": target}))
                .or_else(|| model.by_label(target).map(|p| Found::Pane(Box::new(p))));
            return match found {
                Some(Found::Pane(p)) => ok(
                    id,
                    json!({"remote": true, "address": p.remote.address(),
                    "pane_id": p.pane_id(), "terminal_id": p.terminal_id(),
                    "tools_socket": ctx.tools_socket.display().to_string()}),
                ),
                _ => ok(id, json!({"remote": false})),
            };
        }
        "agent.list" => {
            let remote = model.agents();
            drop(model);
            return merge_list(forward(ctx, upstream, line).await, id, "agent_list", "agents", remote);
        }
        "workspace.list" => {
            drop(model);
            let local = forward(ctx, upstream, line).await;
            let n = local.as_ref().ok().and_then(|v| v["result"]["workspaces"].as_array().map(Vec::len)).unwrap_or(0);
            let remote = ctx.model.read().await.workspaces_json(n);
            return merge_list(local, id, "workspace_list", "workspaces", remote);
        }
        "tab.list" | "pane.list" => {
            let (kind, key) = if method == "tab.list" { ("tab_list", "tabs") } else { ("pane_list", "panes") };
            let pick = |m: &Model, w| if method == "tab.list" { m.tabs_json(w) } else { m.panes_json(w) };
            return match workspace_filter(&model, &params) {
                Some(Some(w)) => ok(id, json!({"type": kind, key: pick(&model, Some(w))})),
                Some(None) => {
                    drop(model);
                    forward(ctx, upstream, line).await.unwrap_or_else(|e| error(id, "server_unavailable", e))
                }
                None => {
                    let remote = pick(&model, None);
                    drop(model);
                    merge_list(forward(ctx, upstream, line).await, id, kind, key, remote)
                }
            };
        }
        "session.snapshot" => {
            drop(model);
            return snapshot(ctx, forward(ctx, upstream, line).await, id).await;
        }
        _ => {}
    }
    if let Some(found) = remote_target(&model, &params) {
        drop(model);
        return remote(ctx, id, method, &params, found).await;
    }
    drop(model);
    let answer = match forward(ctx, upstream, line).await {
        Ok(v) => v,
        Err(e) => return error(id, "server_unavailable", e),
    };
    // An agent named by its tab label that this machine's herdr doesn't have may be another machine's.
    let missing = matches!(answer["error"]["code"].as_str(), Some("agent_not_found" | "agent_name_not_found"));
    if missing
        && let Some(label) = params["target"].as_str()
        && let Some(p) = ctx.model.read().await.by_label(label)
    {
        return remote(ctx, id, method, &params, Found::Pane(Box::new(p))).await;
    }
    answer
}

/// This machine's answer with the other machines' entries added to its list. With this machine's herdr down, the
/// other machines' entries still come back.
fn merge_list(local: Result<Value, String>, id: &Value, kind: &str, key: &str, remote: Vec<Value>) -> Value {
    match local {
        Ok(mut v) if v.get("result").is_some() => {
            if let Some(list) = v["result"][key].as_array_mut() {
                list.extend(remote);
            }
            v
        }
        Ok(v) if remote.is_empty() => v,
        Ok(_) | Err(_) => ok(id, json!({"type": kind, key: remote})),
    }
}

async fn snapshot(ctx: &Ctx, local: Result<Value, String>, id: &Value) -> Value {
    let model = ctx.model.read().await;
    let mut v = match local {
        Ok(v) if v.get("result").is_some() => v,
        Ok(v) if model.len() == 0 => return v,
        Err(e) if model.len() == 0 => return error(id, "server_unavailable", e),
        _ => ok(
            id,
            json!({"type": "session_snapshot", "snapshot": {"workspaces": [], "tabs": [], "panes": [],
            "layouts": [], "agents": []}}),
        ),
    };
    let snap = &mut v["result"]["snapshot"];
    let n = snap["workspaces"].as_array().map_or(0, Vec::len);
    let add = [
        ("workspaces", model.workspaces_json(n)),
        ("tabs", model.tabs_json(None)),
        ("panes", model.panes_json(None)),
        ("layouts", model.layouts_json()),
        ("agents", model.agents()),
    ];
    for (key, items) in add {
        match snap[key].as_array_mut() {
            Some(list) => list.extend(items),
            None => snap[key] = json!(items),
        }
    }
    v
}

fn fail(id: &Value, f: Failure) -> Value {
    error(id, &f.code, f.message)
}

fn unsupported(id: &Value, method: &str) -> Value {
    error(
        id,
        "unsupported_method",
        format!("{method} isn't available for sessions on other machines through Fleetlayer yet"),
    )
}

/// A request about another machine's pane, tab or workspace.
async fn remote(ctx: &Ctx, id: &Value, method: &str, params: &Value, found: Found) -> Value {
    let pane = match found {
        Found::Pane(p) => p,
        Found::Workspace(w) => {
            return match method {
                "workspace.get" | "workspace.focus" => match ctx.model.read().await.workspace_info(w) {
                    Some(ws) => ok(id, json!({"type": "workspace_info", "workspace": ws})),
                    None => error(id, "workspace_not_found", format!("no workspace w{w}")),
                },
                _ => unsupported(id, method),
            };
        }
    };
    let address = pane.remote.address();
    match method {
        "agent.get" | "agent.focus" => ok(id, json!({"type": "agent_info", "agent": pane.agent_json()})),
        "pane.get" | "pane.focus" => ok(id, json!({"type": "pane_info", "pane": pane.pane_json()})),
        "tab.get" | "tab.focus" => ok(id, json!({"type": "tab_info", "tab": pane.tab_json(pane.number as usize)})),
        "agent.read" | "pane.read" => read(ctx, id, params, &pane).await,
        "agent.prompt" => prompt(ctx, id, params, &pane).await,
        "agent.send_keys" | "pane.send_keys" => {
            let keys = strings(&params["keys"]);
            match ctx.pool.keys(&address, &keys).await {
                Ok(()) => ok(id, json!({"type": "ok"})),
                Err(f) => fail(id, f),
            }
        }
        "pane.send_text" | "pane.send_input" => {
            if let Some(text) = params["text"].as_str().filter(|t| !t.is_empty())
                && let Err(f) = ctx.pool.paste(&address, text).await
            {
                return fail(id, f);
            }
            match ctx.pool.keys(&address, &strings(&params["keys"])).await {
                Ok(()) => ok(id, json!({"type": "ok"})),
                Err(f) => fail(id, f),
            }
        }
        "tab.close" | "pane.close" => match ctx.closer.close(&address).await {
            Ok(()) => ok(id, json!({"type": "ok"})),
            Err(f) => fail(id, f),
        },
        _ => unsupported(id, method),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

async fn read(ctx: &Ctx, id: &Value, params: &Value, pane: &Pane) -> Value {
    let source = params["source"].as_str().unwrap_or("visible");
    let ansi = params["format"].as_str() == Some("ansi") && params["strip_ansi"].as_bool() != Some(true);
    let lines = params["lines"].as_u64().map(|n| n as u32).unwrap_or(match source {
        "visible" | "detection" => crate::term::VISIBLE_LINES,
        _ => 200,
    });
    match ctx.pool.read(&pane.remote.address(), lines, ansi).await {
        Ok((text, truncated)) => ok(
            id,
            json!({"type": "pane_read", "read": {"pane_id": pane.pane_id(), "workspace_id": pane.workspace_id(),
                "tab_id": pane.tab_id(), "source": source, "format": if ansi { "ansi" } else { "text" },
                "text": text, "revision": pane.seqs.revision, "truncated": truncated}}),
        ),
        Err(f) => fail(id, f),
    }
}

/// herdr's `agent.prompt` rules: refused while blocked, typed otherwise; with `wait`, until one of `until`.
async fn prompt(ctx: &Ctx, id: &Value, params: &Value, pane: &Pane) -> Value {
    let text = params["text"].as_str().unwrap_or_default();
    if text.is_empty() {
        return error(id, "empty_agent_prompt", "the prompt is empty");
    }
    if pane.remote.status == "blocked" {
        return error(id, "agent_blocked", format!("{} is waiting for a person", pane.label()));
    }
    if let Err(f) = ctx.pool.prompt(&pane.remote.address(), text).await {
        return fail(id, f);
    }
    if params.get("wait").is_none_or(Value::is_null) {
        return ok(id, json!({"type": "agent_prompted", "agent": pane.agent_json()}));
    }
    let wait = &params["wait"];
    let mut until = strings(&wait["until"]);
    if until.is_empty() {
        until = vec!["idle".into(), "done".into(), "blocked".into()];
    }
    let deadline = wait["timeout_ms"].as_u64().map(Duration::from_millis).unwrap_or(Duration::from_secs(30 * 60));
    let start = tokio::time::Instant::now();
    // The listing refreshes every few seconds, so give the turn time to start before a status can end the wait.
    let mut seen_working = false;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = ctx.model.read().await.find(&pane.pane_id());
        let Some(Found::Pane(p)) = now else {
            return error(id, "agent_not_found", format!("{} stopped", pane.label()));
        };
        seen_working |= matches!(p.remote.status.as_str(), "working" | "blocked");
        if (seen_working || start.elapsed() > Duration::from_secs(10)) && until.contains(&p.remote.status) {
            return ok(id, json!({"type": "agent_prompted", "agent": p.agent_json()}));
        }
        if start.elapsed() > deadline {
            return error(id, "timeout", format!("{} didn't reach {} in time", pane.label(), until.join(" or ")));
        }
    }
}

/// One subscription request, split: what this machine's herdr answers, and what's answered here.
#[derive(Debug, Default, PartialEq)]
struct Split {
    local: Vec<Value>,
    /// Lifecycle types to pass for the other machines' sessions.
    lifecycle: Vec<String>,
    /// Status subscriptions for the other machines' sessions: (pane_id filter, agent_status filter).
    status: Vec<(Option<String>, Option<String>)>,
}

fn split(model: &Model, subs: &[Value]) -> Split {
    let mut s = Split::default();
    for sub in subs {
        let kind = sub["type"].as_str().unwrap_or_default();
        let pane = sub["pane_id"].as_str();
        let remote_pane = pane.is_some_and(|p| model.find(p).is_some());
        match kind {
            "pane.agent_status_changed" => {
                if pane.is_none() || remote_pane {
                    s.status.push((pane.map(str::to_string), sub["agent_status"].as_str().map(str::to_string)));
                }
                if !remote_pane {
                    s.local.push(sub.clone());
                }
            }
            "pane.output_matched" | "pane.scroll_changed" => {
                // Not streamed for other machines' panes yet; this machine's still are.
                if !remote_pane {
                    s.local.push(sub.clone());
                }
            }
            _ => {
                s.lifecycle.push(kind.to_string());
                s.local.push(sub.clone());
            }
        }
    }
    s
}

fn wanted(s: &Split, event: &Value) -> bool {
    let kind = subscription_type(event);
    if kind == "pane.agent_status_changed" {
        let data = &event["data"];
        return s.status.iter().any(|(pane, status)| {
            pane.as_deref().is_none_or(|p| data["pane_id"] == p)
                && status.as_deref().is_none_or(|st| data["agent_status"] == st)
        });
    }
    s.lifecycle.contains(&kind)
}

async fn subscribe(ctx: Arc<Ctx>, req: Value, mut w: tokio::net::unix::OwnedWriteHalf) {
    let id = req["id"].clone();
    let subs = req["params"]["subscriptions"].as_array().cloned().unwrap_or_default();
    // Subscribe to the other machines' changes first, so none fall between the answer and the first event.
    let mut remote_rx = ctx.events.subscribe();
    let (s, first_events) = {
        let model = ctx.model.read().await;
        let s = split(&model, &subs);
        // A status subscription for one pane whose status already matches reports it first, as herdr's does.
        let mut first = vec![];
        for (pane, status) in &s.status {
            if let (Some(pane), Some(status)) = (pane, status)
                && let Some(Found::Pane(p)) = model.find(pane)
                && &p.remote.status == status
            {
                first.push(status_event(&p));
            }
        }
        (s, first)
    };
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let mut local_task = None;
    if s.local.is_empty() {
        let started = ok(&id, json!({"type": "subscription_started"}));
        let _ = tx.send(format!("{started}\n")).await;
    } else {
        let mut local_req = req.clone();
        local_req["params"]["subscriptions"] = json!(s.local);
        let up = match Upstream::connect(&ctx.local_socket).await {
            Ok(u) => u,
            Err(e) => {
                let e = error(&id, "server_unavailable", format!("herdr isn't running on this machine ({e})"));
                let _ = w.write_all(format!("{e}\n").as_bytes()).await;
                return;
            }
        };
        let Upstream { mut lines, w: mut uw } = up;
        if uw.write_all(format!("{local_req}\n").as_bytes()).await.is_err() {
            return;
        }
        let first = lines.next_line().await.ok().flatten().unwrap_or_default();
        let v: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
        let _ = tx.send(format!("{first}\n")).await;
        if v.get("error").is_some() {
            drop(tx);
            while let Some(l) = rx.recv().await {
                let _ = w.write_all(l.as_bytes()).await;
            }
            return;
        }
        let tx2 = tx.clone();
        local_task = Some(tokio::spawn(async move {
            let _keep = uw;
            while let Ok(Some(l)) = lines.next_line().await {
                if tx2.send(format!("{l}\n")).await.is_err() {
                    break;
                }
            }
            // herdr closed the stream (it stopped or restarted): the client reconnects, as the contract says.
        }));
    }
    for e in first_events {
        let _ = tx.send(format!("{e}\n")).await;
    }
    let tx3 = tx.clone();
    let remote_task = tokio::spawn(async move {
        loop {
            match remote_rx.recv().await {
                Ok(e) if wanted(&s, &e) => {
                    if tx3.send(format!("{e}\n")).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let lost = json!({"id": Value::Null, "error": {"code": "events_lost",
                        "message": "fell behind the other machines' events"}});
                    let _ = tx3.send(format!("{lost}\n")).await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    drop(tx);
    let local_ended = async {
        match local_task {
            Some(t) => {
                let _ = t.await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(local_ended);
    loop {
        tokio::select! {
            l = rx.recv() => match l {
                Some(l) => if w.write_all(l.as_bytes()).await.is_err() { break },
                None => break,
            },
            _ = &mut local_ended => {
                // Pass on what's queued, then end the connection like herdr would.
                while let Ok(l) = rx.try_recv() {
                    let _ = w.write_all(l.as_bytes()).await;
                }
                break;
            }
        }
    }
    remote_task.abort();
}

#[cfg(test)]
mod tests;
