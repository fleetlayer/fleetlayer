use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use super::*;
use crate::fleet::parse_fleet;

/// A stand-in for this machine's herdr: one workspace with one agent, answers by method, and for a subscription
/// sends `subscription_started` and one local event.
async fn fake_herdr(dir: &Path) -> PathBuf {
    let path = dir.join("herdr.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&l).unwrap();
                    let id = req["id"].clone();
                    let target = req["params"]["target"].as_str().or(req["params"]["pane_id"].as_str());
                    let reply = match req["method"].as_str().unwrap() {
                        "agent.list" => json!({"id": id, "result": {"type": "agent_list", "agents": [
                            {"pane_id": "w1:p1", "agent": "claude", "tab_label": "mine"}]}}),
                        "workspace.list" => json!({"id": id, "result": {"type": "workspace_list", "workspaces": [
                            {"workspace_id": "w1", "number": 1, "label": "here"}]}}),
                        "tab.list" => json!({"id": id, "result": {"type": "tab_list", "tabs": [{"tab_id": "w1:t1"}]}}),
                        "session.snapshot" => json!({"id": id, "result": {"type": "session_snapshot", "snapshot": {
                            "workspaces": [{"workspace_id": "w1"}], "tabs": [], "panes": [], "layouts": [], "agents": []}}}),
                        "agent.read" | "pane.read" if target == Some("w1:p1") => json!({"id": id, "result": {
                            "type": "pane_read", "read": {"pane_id": "w1:p1", "text": "local screen"}}}),
                        "agent.read" | "pane.read" => json!({"id": id, "error": {"code": "agent_not_found",
                            "message": "agent target not found"}}),
                        "events.subscribe" => {
                            let types: Vec<String> = req["params"]["subscriptions"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|s| s["type"].as_str().unwrap().to_string())
                                .collect();
                            w.write_all(
                                format!(
                                    "{}\n",
                                    json!({"id": id, "result": {"type": "subscription_started"},
                                "_types": types})
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                            w.write_all(b"{\"event\":\"tab_renamed\",\"data\":{\"type\":\"tab_renamed\",\"tab_id\":\"w1:t1\"}}\n")
                                .await.unwrap();
                            // Hold the subscription open.
                            while let Ok(Some(_)) = lines.next_line().await {}
                            return;
                        }
                        m => json!({"id": id, "result": {"type": "ok", "_method": m}}),
                    };
                    w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                }
            });
        }
    });
    path
}

/// A stand-in for the node's tools socket: scrollback answers with the session's address; keys and pastes are kept.
async fn fake_node(dir: &Path) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
    let path = dir.join("node.tools.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut lines = BufReader::new(r).lines();
                let open: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let address = open["terminal"].as_str().unwrap().to_string();
                w.write_all(b"{\"ok\":true}\n").await.unwrap();
                while let Ok(Some(l)) = lines.next_line().await {
                    let v: Value = serde_json::from_str(&l).unwrap();
                    log.lock().await.push(format!(
                        "{address} {} {}",
                        open["mode"].as_str().unwrap(),
                        v["type"].as_str().unwrap()
                    ));
                    if v["type"] == "terminal.scrollback" {
                        let r = json!({"type": "terminal.scrollback", "id": v["id"], "text": format!("screen of {address}"),
                            "truncated": false});
                        w.write_all(format!("{r}\n").as_bytes()).await.unwrap();
                    }
                }
            });
        }
    });
    (path, seen)
}

struct FakeCloser(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl Closer for FakeCloser {
    async fn close(&self, address: &str) -> Result<(), Failure> {
        self.0.lock().await.push(address.to_string());
        Ok(())
    }
}

fn fleet() -> Value {
    json!({
        "nodes": [{"name": "here", "online": true}, {"name": "laptop", "online": true}],
        "sessions": [
            {"node": "laptop", "sessionId": "l1", "manager": "herdr", "cwd": "/l",
             "meta": {"harness": "claude", "address": "w1:p6", "workspace": "w1", "tab": "billing", "status": "idle", "running": true}, "ended": null},
            {"node": "laptop", "sessionId": "l2", "manager": "herdr", "cwd": "/l",
             "meta": {"harness": "claude", "address": "w1:p7", "workspace": "w1", "tab": "stuck", "status": "blocked", "running": true}, "ended": null}
        ]
    })
}

struct Rig {
    ctx: Arc<Ctx>,
    seen: Arc<Mutex<Vec<String>>>,
    closed: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let local_socket = fake_herdr(dir.path()).await;
    let (tools_socket, seen) = fake_node(dir.path()).await;
    let mut model = Model::default();
    model.apply(parse_fleet(&fleet(), "here"));
    let closed = Arc::new(Mutex::new(vec![]));
    let (events, _) = broadcast::channel(64);
    let ctx = Arc::new(Ctx {
        model: Arc::new(RwLock::new(model)),
        events,
        local_socket,
        pool: Pool::new(tools_socket.clone()),
        closer: Arc::new(FakeCloser(closed.clone())),
        tools_socket,
    });
    Rig { ctx, seen, closed, _dir: dir }
}

/// One request over a real connection, as a client sends it.
async fn ask(ctx: &Arc<Ctx>, method: &str, params: Value) -> Value {
    let (a, b) = UnixStream::pair().unwrap();
    tokio::spawn(connection(ctx.clone(), b));
    let (r, mut w) = a.into_split();
    w.write_all(format!("{}\n", json!({"id": "t", "method": method, "params": params})).as_bytes()).await.unwrap();
    let line = BufReader::new(r).lines().next_line().await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn lists_show_this_machine_then_the_others() {
    let r = rig().await;
    let agents = ask(&r.ctx, "agent.list", json!({})).await;
    let panes: Vec<&str> =
        agents["result"]["agents"].as_array().unwrap().iter().map(|a| a["pane_id"].as_str().unwrap()).collect();
    assert_eq!(panes, ["w1:p1", "w1000:p1", "w1000:p2"]);
    let ws = ask(&r.ctx, "workspace.list", json!({})).await;
    let ws = ws["result"]["workspaces"].as_array().unwrap();
    assert_eq!((ws[1]["workspace_id"].as_str(), ws[1]["number"].as_u64()), (Some("w1000"), Some(2)));
    assert_eq!(ws[1]["label"], "laptop");
    let tabs = ask(&r.ctx, "tab.list", json!({"workspace_id": "w1000"})).await;
    assert_eq!(tabs["result"]["tabs"].as_array().unwrap().len(), 2, "only that machine's tabs");
    let tabs = ask(&r.ctx, "tab.list", json!({"workspace_id": "w1"})).await;
    assert_eq!(tabs["result"]["tabs"], json!([{"tab_id": "w1:t1"}]), "this machine's herdr answers for its own");
    let snap = ask(&r.ctx, "session.snapshot", json!({})).await;
    let snap = &snap["result"]["snapshot"];
    assert_eq!(snap["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(snap["agents"].as_array().unwrap().len(), 2);
    assert_eq!(snap["layouts"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn this_machines_panes_go_to_its_herdr_unchanged() {
    let r = rig().await;
    let v = ask(&r.ctx, "pane.read", json!({"pane_id": "w1:p1", "source": "visible"})).await;
    assert_eq!(v["result"]["read"]["text"], "local screen");
    let v = ask(&r.ctx, "pane.split", json!({"pane_id": "w1:p1", "direction": "right"})).await;
    assert_eq!(v["result"]["_method"], "pane.split", "anything else passes through");
    assert!(r.seen.lock().await.is_empty(), "nothing went through Fleetlayer");
}

#[tokio::test]
async fn other_machines_panes_are_read_typed_into_and_closed_through_fleetlayer() {
    let r = rig().await;
    let v = ask(
        &r.ctx,
        "pane.read",
        json!({"pane_id": "w1000:p1", "source": "recent_unwrapped", "lines": 500,
        "format": "ansi", "strip_ansi": false}),
    )
    .await;
    let read = &v["result"]["read"];
    assert_eq!(read["text"], "screen of billing@laptop");
    assert_eq!((read["pane_id"].as_str(), read["tab_id"].as_str()), (Some("w1000:p1"), Some("w1000:t1")));
    assert_eq!(read["format"], "ansi");

    // An agent named by its tab label, unknown to this machine's herdr, is found on the other machine.
    let v = ask(&r.ctx, "agent.read", json!({"target": "billing", "source": "visible"})).await;
    assert_eq!(v["result"]["read"]["text"], "screen of billing@laptop");

    let v = ask(&r.ctx, "agent.prompt", json!({"target": "w1000:p1", "text": "run the tests"})).await;
    assert_eq!(v["result"]["type"], "agent_prompted");
    let v = ask(&r.ctx, "agent.send_keys", json!({"target": "w1000:p1", "keys": ["esc"]})).await;
    assert_eq!(v["result"]["type"], "ok");
    let v = ask(&r.ctx, "tab.close", json!({"tab_id": "w1000:t1"})).await;
    assert_eq!(v["result"]["type"], "ok");
    assert_eq!(*r.closed.lock().await, ["billing@laptop"]);

    let seen = r.seen.lock().await.clone();
    assert_eq!(
        seen,
        [
            "billing@laptop observe terminal.scrollback",
            "billing@laptop observe terminal.scrollback",
            "billing@laptop input terminal.paste",
            "billing@laptop input terminal.keys",
            "billing@laptop input terminal.keys",
        ]
    );
}

#[tokio::test]
async fn herdrs_rules_hold_for_other_machines_too() {
    let r = rig().await;
    let v = ask(&r.ctx, "agent.prompt", json!({"target": "w1000:p2", "text": "hello"})).await;
    assert_eq!(v["error"]["code"], "agent_blocked", "a blocked agent gets nothing typed");
    let v = ask(&r.ctx, "agent.prompt", json!({"target": "w1000:p1", "text": ""})).await;
    assert_eq!(v["error"]["code"], "empty_agent_prompt");
    let v = ask(&r.ctx, "pane.split", json!({"pane_id": "w1000:p1", "direction": "right"})).await;
    assert_eq!(v["error"]["code"], "unsupported_method");
    let v = ask(&r.ctx, "workspace.get", json!({"workspace_id": "w1000"})).await;
    assert_eq!(v["result"]["workspace"]["label"], "laptop");
    assert!(r.seen.lock().await.is_empty());
}

#[tokio::test]
async fn the_shim_learns_which_targets_are_another_machines() {
    let r = rig().await;
    let v = ask(&r.ctx, RESOLVE, json!({"target": "w1000:p1"})).await;
    assert_eq!(v["result"]["address"], "billing@laptop");
    assert_eq!(v["result"]["terminal_id"], "fleetlayer-1000-1");
    let v = ask(&r.ctx, RESOLVE, json!({"target": "stuck"})).await;
    assert_eq!(v["result"]["pane_id"], "w1000:p2");
    let v = ask(&r.ctx, RESOLVE, json!({"target": "w1:p1"})).await;
    assert_eq!(v["result"]["remote"], false);
}

#[tokio::test]
async fn one_subscription_carries_both_machines_events() {
    let r = rig().await;
    let (a, b) = UnixStream::pair().unwrap();
    tokio::spawn(connection(r.ctx.clone(), b));
    let (rd, mut w) = a.into_split();
    let req = json!({"id": "s", "method": "events.subscribe", "params": {"subscriptions": [
        {"type": "tab.renamed"}, {"type": "tab.closed"},
        {"type": "pane.agent_status_changed", "agent_status": "done"},
        {"type": "pane.agent_status_changed", "pane_id": "w1000:p2", "agent_status": "blocked"}
    ]}});
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    let started: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(started["result"]["type"], "subscription_started");
    assert_eq!(
        started["_types"],
        json!(["tab.renamed", "tab.closed", "pane.agent_status_changed"]),
        "this machine's herdr isn't asked about another machine's pane"
    );
    let mut got = vec![];
    for _ in 0..2 {
        let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        got.push(format!(
            "{} {}",
            v["event"].as_str().unwrap(),
            v["data"]["pane_id"].as_str().or(v["data"]["tab_id"].as_str()).unwrap()
        ));
    }
    got.sort();
    assert_eq!(
        got,
        ["pane.agent_status_changed w1000:p2", "tab_renamed w1:t1"],
        "the local event, and the blocked pane's current status, which already matches"
    );
    // The laptop's billing session finishes a turn unseen (done), and stuck goes on working: only done passes.
    let mut f = fleet();
    f["sessions"][0]["meta"]["status"] = json!("done");
    f["sessions"][1]["meta"]["status"] = json!("working");
    for e in r.ctx.model.write().await.apply(parse_fleet(&f, "here")) {
        let _ = r.ctx.events.send(e);
    }
    let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(
        (v["event"].as_str(), v["data"]["pane_id"].as_str()),
        (Some("pane.agent_status_changed"), Some("w1000:p1"))
    );
    assert_eq!(v["data"]["agent_status"], "done");
    // The laptop's sessions end: tab.closed passes.
    f["sessions"] = json!([]);
    for e in r.ctx.model.write().await.apply(parse_fleet(&f, "here")) {
        let _ = r.ctx.events.send(e);
    }
    let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(v["event"], "tab_closed");
}
