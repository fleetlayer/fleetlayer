//! `fleetlayer mcp`'s bridge between an MCP client on stdio (Claude Code) and a tool on another node, one JSON-RPC line
//! at a time.
//!
//! - **Each request is timed:** one that gets no response within the limit is answered here with a JSON-RPC error
//!   ("didn't answer within 120s"), and a late response to it is dropped. So a machine that went to sleep or a node
//!   restarting mid-call fails the call instead of leaving the client waiting (Claude Code waits a long time).
//! - **The bridge outlives its link.** When the stream to the tool ends (the other machine asleep, its node restarting
//!   for an upgrade), calls in flight get an error at once and the bridge stays up: the client's next message opens a
//!   new stream, replays the client's `initialize` (and `notifications/initialized`) and goes on. Claude Code never
//!   restarts a stdio MCP server that exited, so a bridge that exited with its link left the session without the tool
//!   until it restarted.
//! - **What doesn't change is answered while there's no link:** the tool's answers to `initialize` (per protocol
//!   version), `server/discover` and `tools/list` are kept (`Answers`), so a session that starts while the other machine
//!   sleeps still loads the tool, and its calls work once the machine is back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::Instant;

pub type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub type Writer = Box<dyn AsyncWrite + Unpin + Send>;
/// Opens a stream to the tool: its reading and writing ends, or why it couldn't.
pub type Connect = Box<dyn FnMut() -> Pin<Box<dyn Future<Output = Result<(Reader, Writer), String>> + Send>> + Send>;

/// How long a new stream has to answer the replayed `initialize`.
const REPLAY_WITHIN: Duration = Duration::from_secs(30);

/// The tool's answers that don't change while it runs, kept in a file so a bridge without a link can answer them.
pub struct Answers {
    path: Option<PathBuf>,
    map: BTreeMap<String, Value>,
}

impl Answers {
    pub fn load(path: Option<PathBuf>) -> Answers {
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Answers { path, map }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn get(&self, request: &Value) -> Option<&Value> {
        self.map.get(&answer_key(request)?)
    }

    fn keep(&mut self, key: String, answer: Value) {
        if self.map.get(&key) == Some(&answer) {
            return;
        }
        self.map.insert(key, answer);
        if let Some(p) = &self.path {
            let tmp = p.with_extension("tmp");
            let _ = p
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&tmp, serde_json::to_vec_pretty(&self.map).unwrap_or_default()))
                .and_then(|()| std::fs::rename(&tmp, p));
        }
    }
}

/// Where a request's answer is kept: `initialize` per protocol version, `server/discover` and `tools/list` (first page)
/// by method. Nothing else: other answers change from call to call.
fn answer_key(v: &Value) -> Option<String> {
    v.get("id").filter(|i| !i.is_null())?;
    match v["method"].as_str()? {
        "initialize" => Some(format!("initialize {}", v["params"]["protocolVersion"].as_str()?)),
        "tools/list" if v["params"]["cursor"].is_null() => Some("tools/list".into()),
        "server/discover" => Some("server/discover".into()),
        _ => None,
    }
}

struct Link {
    down: tokio::io::Lines<BufReader<Reader>>,
    up: Writer,
}

struct Pending {
    id: Value,
    at: Instant,
    keep: Option<String>,
}

async fn write<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, line: &str) -> bool {
    w.write_all(format!("{line}\n").as_bytes()).await.is_ok() && w.flush().await.is_ok()
}

fn error(id: &Value, code: i64, message: String) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// Copy lines both ways until the client is done. `link` is the stream opened already, if one is; `connect` opens
/// another when it ends. `label` names the tool in errors (`browser@laptop`).
pub async fn bridge<I, O>(
    stdin: I,
    mut stdout: O,
    link: Option<(Reader, Writer)>,
    mut connect: Connect,
    mut answers: Answers,
    label: &str,
    limit: Duration,
) where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let mut up = BufReader::new(stdin).lines();
    let mut link = link.map(|(r, w)| Link { down: BufReader::new(r).lines(), up: w });
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut expired: HashSet<String> = HashSet::new();
    let mut init: Option<Value> = None;
    let mut initialized = false;
    let mut stdin_open = true;
    loop {
        if !stdin_open && link.is_none() {
            break;
        }
        let next = pending.values().map(|p| p.at).min();
        tokio::select! {
            // All cancel-safe: a line half read stays buffered.
            line = up.next_line(), if stdin_open => match line {
                Ok(Some(line)) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    let is_request = v.get("method").is_some() && v.get("id").is_some_and(|i| !i.is_null());
                    match v["method"].as_str() {
                        Some("initialize") if is_request => init = Some(v.clone()),
                        Some("notifications/initialized") => initialized = true,
                        _ => {}
                    }
                    if link.is_none() {
                        if is_request && let Some(a) = answers.get(&v) {
                            let mut a = a.clone();
                            a["jsonrpc"] = json!("2.0");
                            a["id"] = v["id"].clone();
                            if !write(&mut stdout, &a.to_string()).await {
                                return;
                            }
                            continue;
                        }
                        if !is_request {
                            // A notification for a server that isn't there; `initialized` is replayed with the next link.
                            continue;
                        }
                        // Replay the client's initialize to the new stream, unless this is it.
                        let replay = init.as_ref().filter(|_| v["method"] != "initialize");
                        match reconnect(&mut connect, replay, initialized, &mut stdout).await {
                            Ok(l) => link = Some(l),
                            Err(why) => {
                                let e = error(&v["id"], -32002, format!(
                                    "{label} can't be reached now ({why}); the machine may be asleep or its node \
                                     restarting. Try again in a while"
                                ));
                                if !write(&mut stdout, &e).await {
                                    return;
                                }
                                continue;
                            }
                        }
                    }
                    if is_request {
                        let id = v["id"].clone();
                        pending.insert(id.to_string(), Pending { id, at: Instant::now() + limit, keep: answer_key(&v) });
                    }
                    let l = link.as_mut().expect("connected above");
                    if !write(&mut l.up, &line).await {
                        // Its end of the stream is gone: the next read says so, and the calls in flight get errors.
                        continue;
                    }
                }
                // The client is done: the tool gets end-of-file and exits, which ends the link.
                _ => {
                    stdin_open = false;
                    if let Some(l) = link.as_mut() {
                        let _ = l.up.shutdown().await;
                    }
                }
            },
            line = async { link.as_mut().expect("guarded").down.next_line().await }, if link.is_some() => match line {
                Ok(Some(line)) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if v.get("method").is_none() && (v.get("result").is_some() || v.get("error").is_some()) {
                        let key = v["id"].to_string();
                        if expired.remove(&key) {
                            continue; // answered already, with the error
                        }
                        if let Some(p) = pending.remove(&key)
                            && let Some(k) = p.keep
                        {
                            let mut a = v.clone();
                            if let Some(o) = a.as_object_mut() {
                                o.remove("id");
                                o.remove("jsonrpc");
                            }
                            answers.keep(k, a);
                        }
                    }
                    if !write(&mut stdout, &line).await {
                        break;
                    }
                }
                _ => {
                    // The stream ended: fail what's in flight now, and open a new one at the client's next message.
                    link = None;
                    for (_, p) in pending.drain() {
                        let e = error(&p.id, -32002, format!(
                            "{label} went away during this call (the machine asleep, or its node restarting); try again"
                        ));
                        if !write(&mut stdout, &e).await {
                            return;
                        }
                    }
                }
            },
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                let now = Instant::now();
                let late: Vec<String> = pending.iter().filter(|(_, p)| p.at <= now).map(|(k, _)| k.clone()).collect();
                for key in late {
                    let p = pending.remove(&key).expect("listed above");
                    let e = error(&p.id, -32001, format!(
                        "{label} didn't answer within {}s (the machine may be asleep, or its node restarting)",
                        limit.as_secs()
                    ));
                    expired.insert(key);
                    if !write(&mut stdout, &e).await {
                        return;
                    }
                }
            }
        }
    }
    let _ = stdout.flush().await;
}

/// Open a new stream and bring the tool to where the client is: its `initialize` again (the answer isn't passed on:
/// the client has it), then `notifications/initialized`.
async fn reconnect<O: AsyncWrite + Unpin>(
    connect: &mut Connect,
    init: Option<&Value>,
    initialized: bool,
    stdout: &mut O,
) -> Result<Link, String> {
    let (r, w) = connect().await?;
    let mut l = Link { down: BufReader::new(r).lines(), up: w };
    let Some(init) = init else { return Ok(l) };
    let mut request = init.clone();
    request["id"] = json!("fleetlayer-reinit");
    if !write(&mut l.up, &request.to_string()).await {
        return Err("the new stream closed at once".into());
    }
    let answered = tokio::time::timeout(REPLAY_WITHIN, async {
        while let Ok(Some(line)) = l.down.next_line().await {
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if v["id"] == "fleetlayer-reinit" {
                return v.get("result").is_some();
            }
            let _ = write(stdout, &line).await;
        }
        false
    })
    .await;
    if answered != Ok(true) {
        return Err("it didn't answer initialize".into());
    }
    if initialized && !write(&mut l.up, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await {
        return Err("the new stream closed at once".into());
    }
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;
    use tokio::sync::mpsc;

    type Lines = tokio::io::Lines<BufReader<tokio::io::DuplexStream>>;
    type Pipe = tokio::io::DuplexStream;

    /// One stream to the tool, from the tool's side: what it reads, and where it writes.
    struct Tool {
        input: Lines,
        output: Pipe,
    }

    impl Tool {
        async fn read(&mut self) -> Value {
            let line =
                tokio::time::timeout(Duration::from_secs(5), self.input.next_line()).await.unwrap().unwrap().unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn answer(&mut self, id: &Value, result: Value) {
            let line = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            self.output.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        }
    }

    struct Client {
        input: Pipe,
        output: Lines,
    }

    impl Client {
        async fn send(&mut self, v: Value) {
            self.input.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        }

        async fn read(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(300), self.output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }
    }

    fn pair() -> ((Reader, Writer), Tool) {
        let (tool_w, from_node) = duplex(1 << 16);
        let (to_node, tool_r) = duplex(1 << 16);
        ((Box::new(from_node), Box::new(to_node)), Tool { input: BufReader::new(tool_r).lines(), output: tool_w })
    }

    /// A bridge whose first stream is open; each later `connect` takes the next result from `next`, and the tool's side
    /// of each stream it opens arrives on the returned receiver.
    fn start(
        limit: Duration,
        answers: Answers,
        first: bool,
    ) -> (Client, Option<Tool>, mpsc::UnboundedSender<bool>, mpsc::UnboundedReceiver<Tool>, tokio::task::JoinHandle<()>)
    {
        let (client_w, stdin) = duplex(1 << 16);
        let (stdout, client_r) = duplex(1 << 16);
        let (link, tool) = if first {
            let (l, t) = pair();
            (Some(l), Some(t))
        } else {
            (None, None)
        };
        let (next_tx, next_rx) = mpsc::unbounded_channel::<bool>();
        let (tools_tx, tools_rx) = mpsc::unbounded_channel::<Tool>();
        let next_rx = std::sync::Arc::new(tokio::sync::Mutex::new(next_rx));
        let connect: Connect = Box::new(move || {
            let (next_rx, tools_tx) = (next_rx.clone(), tools_tx.clone());
            Box::pin(async move {
                let up = next_rx.lock().await.recv().await.unwrap_or(false);
                if !up {
                    return Err("laptop is asleep".to_string());
                }
                let (l, t) = pair();
                tools_tx.send(t).unwrap();
                Ok(l)
            })
        });
        let task =
            tokio::spawn(async move { bridge(stdin, stdout, link, connect, answers, "browser@laptop", limit).await });
        (Client { input: client_w, output: BufReader::new(client_r).lines() }, tool, next_tx, tools_rx, task)
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#;

    #[tokio::test(start_paused = true)]
    async fn a_call_that_never_answers_gets_an_error_and_later_calls_still_work() {
        let (mut client, tool, _next, _tools, _task) = start(Duration::from_secs(120), Answers::load(None), true);
        let mut tool = tool.unwrap();
        client.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call"})).await;
        assert_eq!(tool.read().await["id"], 1);
        // Nothing comes back: after 120 s the client gets an error for id 1.
        let v = client.read().await;
        assert_eq!(v["id"], 1);
        assert_eq!(
            v["error"]["message"],
            "browser@laptop didn't answer within 120s (the machine may be asleep, or its node restarting)"
        );
        // The late answer is dropped; the next call's answer goes through.
        tool.answer(&json!(1), json!({})).await;
        client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call"})).await;
        assert_eq!(tool.read().await["id"], 2);
        tool.answer(&json!(2), json!({"ok": true})).await;
        let v = client.read().await;
        assert_eq!((v["id"].clone(), v["result"]["ok"].clone()), (json!(2), json!(true)));
    }

    #[tokio::test(start_paused = true)]
    async fn when_the_link_ends_the_next_call_reconnects_and_replays_initialize() {
        let (mut client, tool, next, mut tools, task) = start(Duration::from_secs(120), Answers::load(None), true);
        let mut tool = tool.unwrap();
        client.send(serde_json::from_str(INIT).unwrap()).await;
        assert_eq!(tool.read().await["method"], "initialize");
        tool.answer(&json!(0), json!({"protocolVersion": "2025-11-25"})).await;
        assert_eq!(client.read().await["id"], 0);
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        tool.read().await;
        // A call in flight when the other node restarts gets an error at once, not after the time limit.
        client.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call"})).await;
        tool.read().await;
        drop(tool);
        let v = client.read().await;
        assert_eq!(v["id"], 1);
        assert!(v["error"]["message"].as_str().unwrap().contains("went away during this call"), "{v}");
        assert!(!task.is_finished(), "the bridge stays up for the client");
        // The next call opens a new stream, brings the tool up to date, then goes through.
        next.send(true).unwrap();
        client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_tabs"}})).await;
        let mut tool = tools.recv().await.unwrap();
        let replay = tool.read().await;
        assert_eq!((replay["method"].clone(), replay["id"].clone()), (json!("initialize"), json!("fleetlayer-reinit")));
        assert_eq!(replay["params"]["protocolVersion"], "2025-11-25");
        tool.answer(&json!("fleetlayer-reinit"), json!({"protocolVersion": "2025-11-25"})).await;
        assert_eq!(tool.read().await["method"], "notifications/initialized");
        assert_eq!(tool.read().await["id"], 2);
        tool.answer(&json!(2), json!({"ok": true})).await;
        let v = client.read().await;
        assert_eq!(v["id"], 2, "the client never sees the replayed initialize's answer: {v}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_reconnect_answers_with_an_error_and_a_later_one_works() {
        let (mut client, tool, next, mut tools, task) = start(Duration::from_secs(120), Answers::load(None), true);
        drop(tool);
        tokio::time::sleep(Duration::from_millis(10)).await;
        next.send(false).unwrap();
        client.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call"})).await;
        let v = client.read().await;
        assert!(v["error"]["message"].as_str().unwrap().contains("can't be reached now (laptop is asleep)"), "{v}");
        assert!(!task.is_finished());
        next.send(true).unwrap();
        client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call"})).await;
        let mut tool = tools.recv().await.unwrap();
        assert_eq!(tool.read().await["id"], 2, "no initialize to replay: the client never sent one");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_starts_while_the_machine_sleeps_still_loads_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("browser@laptop.json");
        // An earlier session saw the tool's answers.
        {
            let (mut client, tool, _next, _tools, _task) =
                start(Duration::from_secs(120), Answers::load(Some(path.clone())), true);
            let mut tool = tool.unwrap();
            client.send(serde_json::from_str(INIT).unwrap()).await;
            tool.read().await;
            tool.answer(&json!(0), json!({"protocolVersion": "2025-11-25", "serverInfo": {"name": "Playwright"}}))
                .await;
            client.read().await;
            client.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
            tool.read().await;
            tool.answer(&json!(1), json!({"tools": [{"name": "browser_navigate"}]})).await;
            client.read().await;
        }
        // Now the laptop is asleep: no stream at all, and the answers come from what was kept.
        let (mut client, _none, next, _tools, task) = start(Duration::from_secs(120), Answers::load(Some(path)), false);
        client.send(serde_json::from_str(INIT).unwrap()).await;
        let v = client.read().await;
        assert_eq!((v["id"].clone(), v["result"]["serverInfo"]["name"].clone()), (json!(0), json!("Playwright")));
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        client.send(json!({"jsonrpc":"2.0","id":"t","method":"tools/list"})).await;
        let v = client.read().await;
        assert_eq!((v["id"].clone(), v["result"]["tools"][0]["name"].clone()), (json!("t"), json!("browser_navigate")));
        // A call while it sleeps fails with a reason, and the session keeps the tool.
        next.send(false).unwrap();
        client.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call"})).await;
        assert!(client.read().await["error"]["message"].as_str().unwrap().contains("can't be reached now"));
        assert!(!task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn answers_in_time_and_notifications_pass_unchanged() {
        let (mut client, tool, _next, _tools, _task) = start(Duration::from_secs(120), Answers::load(None), true);
        let mut tool = tool.unwrap();
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        client.send(json!({"jsonrpc":"2.0","id":"a","method":"tools/list"})).await;
        tool.read().await;
        tool.read().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        tool.output.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n").await.unwrap();
        tool.answer(&json!("a"), json!({"tools": []})).await;
        assert_eq!(client.read().await["method"], "notifications/progress");
        assert!(client.read().await["result"]["tools"].is_array());
    }

    #[tokio::test(start_paused = true)]
    async fn the_client_leaving_ends_the_bridge() {
        let (client, tool, _next, _tools, task) = start(Duration::from_secs(120), Answers::load(None), true);
        let mut tool = tool.unwrap();
        drop(client);
        // The tool gets end-of-file, exits, and the bridge ends.
        assert!(tool.input.next_line().await.unwrap().is_none());
        drop(tool);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }
}
