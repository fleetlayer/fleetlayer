//! A shared MCP server starts at its caller's first tool call, not when the caller connects. A session starts its MCP
//! servers at launch whether or not it will use them, so a laptop sharing its browser with twenty sessions would run
//! twenty idle Playwright processes. The node answers `initialize`, `tools/list` and `ping` from what the server
//! answered last time (a cache per tool, beside its working directory), and starts the server only when something
//! else arrives: it replays the caller's `initialize`, then passes everything through.
//!
//! After each start the node asks the server for its tools itself; when they (or the server's `initialize` answer, such
//! as its version) have changed since the cache was written, it rewrites the cache and tells the caller with
//! `notifications/tools/list_changed`. With no cache, or one written for other settings, the server starts at once
//! and fills the cache from that connection. `lazy: false` in a tool's settings turns this off.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};

/// How long a started server has to answer the replayed `initialize` and the node's own `tools/list`.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the server's last output once the caller has left.
const DRAIN: Duration = Duration::from_secs(10);

/// How to start the server.
pub struct Spawn {
    pub tool: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: PathBuf,
}

impl Spawn {
    fn start(&self) -> std::io::Result<tokio::process::Child> {
        let mut child = tokio::process::Command::new(&self.command)
            .args(&self.args)
            .envs(&self.env)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(stderr) = child.stderr.take() {
            let tool = self.tool.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(tool = %tool, "stderr: {line}");
                }
            });
        }
        Ok(child)
    }
}

/// What the server answered last time, for the settings in `key`: each answer (`{"result": …}` or `{"error": …}`)
/// under the request it answers (`answer_key`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    pub key: String,
    pub answers: BTreeMap<String, Value>,
}

/// Where a request's answer is kept: `initialize` per protocol version the caller asks for, and the discovery probe
/// (`server/discover`, which Claude Code sends before `initialize`) and `tools/list` by method. Nothing else: other
/// answers can change from call to call, or the request does something.
fn answer_key(v: &Value) -> Option<String> {
    v.get("id")?;
    match v["method"].as_str()? {
        "initialize" => Some(format!("initialize {}", v["params"]["protocolVersion"].as_str()?)),
        "tools/list" if v["params"]["cursor"].is_null() => Some("tools/list".into()),
        "server/discover" => Some("server/discover".into()),
        _ => None,
    }
}

/// The answer part of a response (`result` or `error`), to keep.
fn answer_of(v: &Value) -> Option<Value> {
    match (v.get("result"), v.get("error")) {
        (Some(r), _) => Some(json!({"result": r})),
        (None, Some(e)) => Some(json!({"error": e})),
        _ => None,
    }
}

/// The settings a cache was written for: the command, its arguments (before `{session}` is filled in), the
/// environment set in the settings (never what `env_files` reads) and the working directory.
pub fn key(command: &str, args: &[String], env: &HashMap<String, String>, cwd: &std::path::Path) -> String {
    let mut env: Vec<_> = env.iter().collect();
    env.sort();
    let text = json!({"command": command, "args": args, "env": env, "cwd": cwd}).to_string();
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The cache for these settings, if one was written; an empty one counts as none.
pub fn load(path: &std::path::Path, key: &str) -> Option<Cache> {
    let cache: Cache = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (cache.key == key && !cache.answers.is_empty()).then_some(cache)
}

fn save(path: &std::path::Path, cache: &Cache) {
    let tmp = path.with_extension("tmp");
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec_pretty(cache).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        tracing::warn!("writing {}: {e}", path.display());
    }
}

/// Start serving: answers the ends the caller's pipe reads from and writes to. With `cache`, the server starts at the
/// caller's first message the cache can't answer; without, it starts now (and a failure to start is answered now).
pub fn serve(
    spawn: Spawn,
    cache: Option<Cache>,
    path: PathBuf,
    key: String,
) -> std::io::Result<(tokio::io::ReadHalf<DuplexStream>, tokio::io::WriteHalf<DuplexStream>)> {
    let (pipe, front_end) = tokio::io::duplex(1 << 16);
    let child = match cache {
        Some(_) => None,
        None => Some(spawn.start()?),
    };
    tokio::spawn(front(front_end, spawn, child, cache, path, key));
    Ok(tokio::io::split(pipe))
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> bool {
    w.write_all(format!("{v}\n").as_bytes()).await.is_ok() && w.flush().await.is_ok()
}

async fn reply<W: AsyncWrite + Unpin>(w: &mut W, id: &Value, answer: &Value) -> bool {
    let mut v = answer.clone();
    v["jsonrpc"] = json!("2.0");
    v["id"] = id.clone();
    send(w, &v).await
}

/// The cache being kept up to date from what passes through: requests whose answers are kept, by id.
struct Keeper {
    cache: Cache,
    path: PathBuf,
    pending: HashMap<String, String>,
}

impl Keeper {
    fn asked(&mut self, v: &Value) {
        if let Some(k) = answer_key(v) {
            self.pending.insert(v["id"].to_string(), k);
        }
    }

    fn answered(&mut self, v: &Value) {
        if v.get("method").is_some() {
            return;
        }
        let Some(id) = v.get("id") else { return };
        let Some(k) = self.pending.remove(&id.to_string()) else { return };
        if let Some(a) = answer_of(v)
            && self.cache.answers.get(&k) != Some(&a)
        {
            self.cache.answers.insert(k, a);
            save(&self.path, &self.cache);
        }
    }
}

async fn front(
    io: DuplexStream,
    spawn: Spawn,
    child: Option<tokio::process::Child>,
    cache: Option<Cache>,
    path: PathBuf,
    key: String,
) {
    let (r, mut w) = tokio::io::split(io);
    let mut lines = BufReader::new(r).lines();
    // Answer what the cache can until something else arrives.
    let mut init: Option<Value> = None;
    let mut initialized = false;
    let mut first: Option<String> = None;
    if let Some(cache) = &cache {
        loop {
            let Ok(Some(line)) = lines.next_line().await else {
                // The caller left without a tool call: nothing was started.
                return;
            };
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let cached = answer_key(&v).and_then(|k| cache.answers.get(&k));
            let answered = match (cached, v["method"].as_str()) {
                (Some(a), m) => {
                    if m == Some("initialize") {
                        init = Some(v.clone());
                    }
                    reply(&mut w, &v["id"], a).await
                }
                (None, Some("notifications/initialized")) if init.is_some() => {
                    initialized = true;
                    true
                }
                (None, Some("ping")) => reply(&mut w, &v["id"], &json!({"result": {}})).await,
                _ => {
                    first = Some(line);
                    break;
                }
            };
            if !answered {
                return;
            }
        }
    }
    let mut child = match child {
        Some(c) => c,
        None => {
            let why = first
                .as_deref()
                .and_then(|l| serde_json::from_str::<Value>(l).ok())
                .and_then(|v| v["method"].as_str().map(str::to_string))
                .unwrap_or_else(|| "a message".into());
            tracing::info!(tool = %spawn.tool, "starting the server for the caller's {why}");
            match spawn.start() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(tool = %spawn.tool, "starting {}: {e}", spawn.command);
                    if let Some(id) =
                        first.as_deref().and_then(|l| serde_json::from_str::<Value>(l).ok()).map(|v| v["id"].clone())
                        && !id.is_null()
                    {
                        let error = json!({"code": -32603, "message": format!("{} couldn't start: {e}", spawn.tool)});
                        send(&mut w, &json!({"jsonrpc": "2.0", "id": id, "error": error})).await;
                    }
                    return;
                }
            }
        }
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    let mut out = BufReader::new(stdout).lines();
    let mut keeper = Keeper {
        cache: cache.clone().unwrap_or(Cache { key, answers: BTreeMap::new() }),
        path,
        pending: HashMap::new(),
    };
    if let Some(init) = &init {
        // The caller's initialize was answered from the cache: bring the server to the same point, then check its
        // answers are still the ones kept.
        let Some((initialize, tools)) = replay(&mut stdin, &mut out, &mut w, init, initialized).await else {
            tracing::warn!(tool = %spawn.tool, "the server didn't answer initialize and tools/list in time");
            return;
        };
        let init_key = answer_key(init).unwrap_or_default();
        let fresh = |k: &str, a: Value| (k.to_string(), json!({"result": a}));
        let (ik, ia) = fresh(&init_key, initialize);
        let (tk, ta) = fresh("tools/list", tools);
        let tools_changed = keeper.cache.answers.get(&tk) != Some(&ta);
        if keeper.cache.answers.get(&ik) != Some(&ia) {
            // Another server (an upgrade): what it answered before is stale.
            keeper.cache.answers.clear();
        }
        if tools_changed || !keeper.cache.answers.contains_key(&ik) {
            keeper.cache.answers.insert(ik, ia);
            keeper.cache.answers.insert(tk, ta);
            save(&keeper.path, &keeper.cache);
        }
        if tools_changed {
            send(&mut w, &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})).await;
        }
    }
    let keeper = Arc::new(Mutex::new(keeper));
    if let Some(line) = first {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            keeper.lock().unwrap_or_else(|e| e.into_inner()).asked(&v);
        }
        if !send_line(&mut stdin, &line).await {
            return;
        }
    }
    // Pass everything through, keeping the answers the cache holds up to date.
    let up_keeper = keeper.clone();
    let up = async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                up_keeper.lock().unwrap_or_else(|e| e.into_inner()).asked(&v);
            }
            if !send_line(&mut stdin, &line).await {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    };
    let down = async {
        while let Ok(Some(line)) = out.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                keeper.lock().unwrap_or_else(|e| e.into_inner()).answered(&v);
            }
            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
        let _ = w.shutdown().await;
    };
    tokio::pin!(up, down);
    tokio::select! {
        () = &mut up => { let _ = tokio::time::timeout(DRAIN, &mut down).await; }
        () = &mut down => {}
    }
    let _ = child.kill().await;
}

async fn send_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> bool {
    w.write_all(format!("{line}\n").as_bytes()).await.is_ok() && w.flush().await.is_ok()
}

/// Replay the caller's `initialize` (and `notifications/initialized`) to a server started late, then ask it for its
/// tools: answers both results. Anything else the server says meanwhile goes to the caller.
async fn replay<W1, W2, R>(
    stdin: &mut W1,
    out: &mut tokio::io::Lines<R>,
    caller: &mut W2,
    init: &Value,
    initialized: bool,
) -> Option<(Value, Value)>
where
    W1: AsyncWrite + Unpin,
    W2: AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut request = init.clone();
    request["id"] = json!("fleetlayer-init");
    if !send(stdin, &request).await {
        return None;
    }
    let initialize = answer(out, caller, "fleetlayer-init").await?;
    if initialized && !send(stdin, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await {
        return None;
    }
    if !send(stdin, &json!({"jsonrpc": "2.0", "id": "fleetlayer-list", "method": "tools/list", "params": {}})).await {
        return None;
    }
    let tools = answer(out, caller, "fleetlayer-list").await?;
    Some((initialize, tools))
}

async fn answer<W, R>(out: &mut tokio::io::Lines<R>, caller: &mut W, id: &str) -> Option<Value>
where
    W: AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    let wait = async {
        while let Ok(Some(line)) = out.next_line().await {
            match serde_json::from_str::<Value>(&line) {
                Ok(v) if v["id"] == id => return v.get("result").cloned(),
                _ => {
                    let _ = send_line(caller, &line).await;
                }
            }
        }
        None
    };
    tokio::time::timeout(START_TIMEOUT, wait).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// A stand-in MCP server: notes that it started and every line it gets, answers initialize with `$VERSION` and
    /// tools/list with one tool, `$TOOL`.
    const FAKE: &str = r#"echo started >> "$LOG"
      while IFS= read -r l; do
        printf '%s\n' "$l" >> "$LOG"
        id=$(printf '%s' "$l" | sed -nE 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
        case "$l" in
          *server/discover*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id" ;;
          *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"fake","version":"%s"}}}\n' "$id" "$VERSION" ;;
          *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"%s"}]}}\n' "$id" "$TOOL" ;;
          *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[]}}\n' "$id" ;;
        esac
      done"#;

    struct Fake {
        dir: tempfile::TempDir,
        log: PathBuf,
        cache: PathBuf,
    }

    impl Fake {
        fn new() -> Fake {
            let dir = tempfile::tempdir().unwrap();
            let (log, cache) = (dir.path().join("log"), dir.path().join("browser.mcp-cache.json"));
            std::fs::write(&log, "").unwrap();
            Fake { dir, log, cache }
        }

        fn spawn(&self, version: &str, tool: &str) -> Spawn {
            let env = HashMap::from([
                ("LOG".to_string(), self.log.to_string_lossy().into_owned()),
                ("VERSION".to_string(), version.to_string()),
                ("TOOL".to_string(), tool.to_string()),
            ]);
            Spawn {
                tool: "browser".into(),
                command: "sh".into(),
                args: vec!["-c".into(), FAKE.into()],
                env,
                cwd: self.dir.path().into(),
            }
        }

        fn serve(&self, version: &str, tool: &str) -> Conn {
            let cache = load(&self.cache, "k");
            let (r, w) = serve(self.spawn(version, tool), cache, self.cache.clone(), "k".into()).unwrap();
            Conn { lines: BufReader::new(r).lines(), w }
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap()
        }
    }

    struct Conn {
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
        w: tokio::io::WriteHalf<DuplexStream>,
    }

    impl Conn {
        async fn send(&mut self, v: Value) {
            assert!(send(&mut self.w, &v).await);
        }

        async fn next(&mut self) -> Value {
            let line =
                tokio::time::timeout(Duration::from_secs(10), self.lines.next_line()).await.unwrap().unwrap().unwrap();
            serde_json::from_str(&line).unwrap()
        }

        /// initialize, initialized, tools/list: answers the two results.
        async fn start(&mut self) -> (Value, Value) {
            self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}})).await;
            let init = self.next().await;
            self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
            self.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
            let tools = self.next().await;
            assert_eq!((init["id"].clone(), tools["id"].clone()), (json!(1), json!(2)));
            (init["result"].clone(), tools["result"].clone())
        }

        /// What Claude Code 2.1.332 sends at launch: a `server/discover` probe, then initialize at 2025-11-25,
        /// initialized and tools/list. Answers the tools/list result.
        async fn claude_code_start(&mut self) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}})).await;
            assert_eq!(self.next().await["id"], "server-discover-probe-1");
            self.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true}},"clientInfo":{"name":"claude-code","version":"2.1.332"}}})).await;
            assert_eq!(self.next().await["id"], 0);
            self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
            self.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
            let tools = self.next().await;
            assert_eq!(tools["id"], 1);
            tools["result"].clone()
        }

        async fn leave(mut self) {
            self.w.shutdown().await.unwrap();
            let mut rest = String::new();
            let mut r = self.lines.into_inner();
            let _ = tokio::time::timeout(Duration::from_secs(15), r.read_to_string(&mut rest)).await;
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn with_no_cache_the_server_starts_at_once_and_fills_it() {
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        let (init, tools) = c.start().await;
        assert_eq!(init["serverInfo"]["version"], "1");
        c.leave().await;
        let cache = load(&f.cache, "k").expect("a cache");
        assert_eq!(cache.answers["initialize 2025-06-18"]["result"], init);
        assert_eq!(cache.answers["tools/list"]["result"], tools);
        assert!(load(&f.cache, "other settings").is_none(), "a cache is only for the settings it was written for");
    }

    #[tokio::test]
    async fn with_a_cache_a_session_that_never_calls_a_tool_starts_nothing() {
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        let first = c.start().await;
        c.leave().await;
        std::fs::write(&f.log, "").unwrap();
        let mut c = f.serve("1", "browser_navigate");
        assert_eq!(c.start().await, first, "answered from the cache");
        c.send(json!({"jsonrpc":"2.0","id":3,"method":"ping"})).await;
        assert_eq!(c.next().await["id"], 3);
        c.leave().await;
        settle().await;
        assert_eq!(f.log(), "", "the server never started");
    }

    #[tokio::test]
    async fn the_first_tool_call_starts_the_server_and_a_changed_server_refreshes_the_cache() {
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        c.start().await;
        c.leave().await;
        std::fs::write(&f.log, "").unwrap();
        // The server was upgraded: a new version with another tool.
        let mut c = f.serve("2", "browser_find");
        let (init, tools) = c.start().await;
        assert_eq!(
            (init["serverInfo"]["version"].clone(), tools["tools"][0]["name"].clone()),
            (json!("1"), json!("browser_navigate"))
        );
        settle().await;
        assert_eq!(f.log(), "", "not started by initialize or tools/list");
        c.send(
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_navigate","arguments":{}}}),
        )
        .await;
        assert_eq!(c.next().await["method"], "notifications/tools/list_changed", "the caller hears the tools changed");
        assert_eq!(c.next().await["id"], 4, "then the call's answer");
        let log = f.log();
        let order: Vec<&str> =
            ["started", "fleetlayer-init", "notifications/initialized", "fleetlayer-list", "tools/call"]
                .into_iter()
                .filter(|s| log.contains(s))
                .collect();
        assert_eq!(order.len(), 5, "{log}");
        let at = |s: &str| log.find(s).unwrap();
        assert!(at("started") < at("fleetlayer-init") && at("fleetlayer-init") < at("notifications/initialized"));
        assert!(
            at("notifications/initialized") < at("fleetlayer-list") && at("fleetlayer-list") < at("tools/call"),
            "{log}"
        );
        c.leave().await;
        let cache = load(&f.cache, "k").unwrap();
        assert_eq!(cache.answers["initialize 2025-06-18"]["result"]["serverInfo"]["version"], "2");
        assert_eq!(cache.answers["tools/list"]["result"]["tools"][0]["name"], "browser_find");
    }

    #[tokio::test]
    async fn claude_codes_discover_probe_starts_nothing_once_cached() {
        // Claude Code opens with server/discover before initialize: every idle session started a server.
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        let first = c.claude_code_start().await;
        c.leave().await;
        std::fs::write(&f.log, "").unwrap();
        let mut c = f.serve("1", "browser_navigate");
        assert_eq!(c.claude_code_start().await, first, "answered from the cache");
        c.leave().await;
        settle().await;
        assert_eq!(f.log(), "", "the server never started");
    }

    #[tokio::test]
    async fn a_second_protocol_version_is_cached_too() {
        // A client asking for another protocol version starts the server once, and its answers join the cache.
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        c.claude_code_start().await;
        c.leave().await;
        let mut c = f.serve("1", "browser_navigate");
        c.start().await;
        c.leave().await;
        std::fs::write(&f.log, "").unwrap();
        for _ in 0..2 {
            let mut c = f.serve("1", "browser_navigate");
            c.start().await;
            c.leave().await;
            let mut c = f.serve("1", "browser_navigate");
            c.claude_code_start().await;
            c.leave().await;
        }
        settle().await;
        assert_eq!(f.log(), "", "both clients answered from the cache");
    }

    #[tokio::test]
    async fn a_caller_asking_for_another_protocol_version_starts_the_server() {
        let f = Fake::new();
        let mut c = f.serve("1", "browser_navigate");
        c.start().await;
        c.leave().await;
        std::fs::write(&f.log, "").unwrap();
        let mut c = f.serve("1", "browser_navigate");
        c.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}})).await;
        assert_eq!(c.next().await["id"], 1);
        c.leave().await;
        assert!(f.log().starts_with("started"), "{}", f.log());
    }
}
