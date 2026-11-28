//! fleetlayer-tmux against a real tmux: a private server (its own socket, `-f /dev/null`, a throwaway HOME and
//! TMUX_TMPDIR), agents that are a fake `claude` writing Claude Code's session registry, and the plugin driven over
//! JSON-RPC on stdio as the node drives it. Never a real Claude Code.
//!
//! Skipped when tmux isn't installed, unless FLEETLAYER_REQUIRE_TMUX is set (CI sets it).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

const FAKE: &str = r#"#!/bin/sh
# A stand-in for Claude Code: it registers itself the way Claude Code does, reports its state there, and echoes.
reg() {
  printf '{"pid":%s,"sessionId":"fake-%s","status":"%s","name":"%s","nameSource":"user"}' $$ $$ "$1" "$NAME" > "SESSIONS/$$.tmp"
  mv "SESSIONS/$$.tmp" "SESSIONS/$$.json"
}
NAME=""
BLOCKED=""
while [ $# -gt 0 ]; do
  case $1 in
    --name) NAME=$2; shift ;;
    --blocked) BLOCKED=1 ;;
  esac
  shift
done
if [ -n "$BLOCKED" ]; then
  reg waiting
  printf ' Do you want to proceed?\n ❯ 1. Yes\n   2. No\n'
  IFS= read -r line
  echo "picked: $line"
  reg idle
fi
reg idle
printf 'ready> '
while IFS= read -r line; do
  [ "$line" = exit ] && { rm -f "SESSIONS/$$.json"; exit 0; }
  reg busy
  echo "heard: $line"
  sleep 1
  reg idle
  printf 'ready> '
done
"#;

struct Sandbox {
    dir: PathBuf,
    socket: String,
    tmux: String,
}

impl Sandbox {
    fn new() -> Option<Sandbox> {
        let tmux = std::env::var("FLEETLAYER_TMUX").unwrap_or_else(|_| "tmux".into());
        let found = Command::new(&tmux).arg("-V").output().is_ok_and(|o| o.status.success());
        if !found {
            assert!(std::env::var_os("FLEETLAYER_REQUIRE_TMUX").is_none(), "tmux isn't installed, and CI needs it");
            eprintln!("skipped: tmux isn't installed");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("fleetlayer-tmux-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["bin", "home", "claude/sessions", "tmp", "work"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let sessions = dir.join("claude/sessions");
        let fake = dir.join("bin/claude");
        std::fs::write(&fake, FAKE.replace("SESSIONS", &sessions.display().to_string())).unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // A login shell resets PATH from /etc/profile, then reads this.
        std::fs::write(
            dir.join("home/.profile"),
            format!("PATH=\"{}:$PATH\"\nexport PATH\n", dir.join("bin").display()),
        )
        .unwrap();
        let sb = Sandbox { socket: format!("fl-test-{}", std::process::id()), dir, tmux };
        // The person's own tmux: an agent under a shell that stays when it exits, and a window with no agent.
        sb.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "person",
            "-x",
            "120",
            "-y",
            "40",
            "--",
            "/bin/sh",
            "-c",
            &format!("{} --name planning; echo agent gone; sleep 600", fake.display()),
        ]);
        sb.tmux(&["new-window", "-d", "-t", "person:", "-n", "notes", "--", "sleep", "600"]);
        sb.tmux(&[
            "new-window",
            "-d",
            "-t",
            "person:",
            "-n",
            "approvals",
            "--",
            &fake.display().to_string(),
            "--blocked",
        ]);
        Some(sb)
    }

    fn env(&self, c: &mut Command) {
        c.env("HOME", self.dir.join("home"))
            .env("TMUX_TMPDIR", self.dir.join("tmp"))
            .env("PATH", format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env_remove("TMUX")
            .env_remove("CLAUDE_CONFIG_DIR");
    }

    fn tmux(&self, args: &[&str]) -> String {
        let mut c = Command::new(&self.tmux);
        c.args(["-L", &self.socket]).args(args);
        self.env(&mut c);
        let out = c.output().unwrap();
        assert!(out.status.success(), "tmux {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let mut c = Command::new(&self.tmux);
        c.args(["-L", &self.socket, "kill-server"]);
        self.env(&mut c);
        let _ = c.output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The plugin, driven as the node drives it.
struct Driver {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    notes: Vec<Value>,
    next: u64,
}

impl Driver {
    fn start(sb: &Sandbox) -> Driver {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fleetlayer-tmux"));
        sb.env(&mut c);
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = c.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Driver { child, stdin, lines: rx, notes: vec![], next: 1 }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let v = self.lines.recv_timeout(deadline - Instant::now()).expect("the plugin answers");
            if v.get("id") == Some(&json!(id)) {
                return match v.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(v["result"].clone()),
                };
            }
            if v.get("method").is_some() && v.get("id").is_none() {
                self.notes.push(v);
            }
        }
    }

    /// Wait for a notification that `f` accepts, keeping the others.
    fn wait_note(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.notes.iter().position(&f) {
            return self.notes.remove(i);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(v) = self.lines.recv_timeout(Duration::from_millis(200)) {
                if f(&v) {
                    return v;
                }
                self.notes.push(v);
            }
        }
        panic!("no {what} within 20 seconds; heard {:?}", self.notes);
    }

    fn list(&mut self) -> Vec<Value> {
        self.call("session/list", json!({})).unwrap()["sessions"].as_array().cloned().unwrap()
    }

    fn session(&mut self, tab: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(s) = self.list().into_iter().find(|s| meta(s)["tab"] == tab) {
                return s;
            }
            assert!(Instant::now() < deadline, "no session {tab} within 20 seconds");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn screen(&mut self, sid: &str) -> String {
        self.call("_fleetlayer.dev/session/screen", json!({"sessionId": sid, "lines": 40})).unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn wait_screen(&mut self, sid: &str, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = self.screen(sid);
            if s.contains(text) {
                return s;
            }
            assert!(Instant::now() < deadline, "{sid}'s screen never showed {text:?}: {s}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn wait_status(&mut self, tab: &str, status: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = self.session(tab);
            if meta(&s)["status"] == status {
                return;
            }
            assert!(Instant::now() < deadline, "{tab} never became {status}: {s}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn meta(s: &Value) -> &Value {
    &s["_meta"]["fleetlayer.dev/session"]
}

fn deliver(d: &mut Driver, s: &Value, text: &str) -> Result<Value, Value> {
    d.call(
        "_fleetlayer.dev/session/deliver",
        json!({"sessionId": s["sessionId"], "address": meta(s)["address"], "deliveryId": "d1", "kind": "message",
               "from": {"label": "test"}, "text": text}),
    )
}

fn code(e: &Value) -> i64 {
    e["code"].as_i64().unwrap()
}

fn sid(s: &Value) -> String {
    s["sessionId"].as_str().unwrap().to_string()
}

#[test]
fn the_plugin_lists_reaches_and_streams_agents_in_a_real_tmux() {
    let Some(sb) = Sandbox::new() else { return };
    let windows_before = sb.tmux(&["list-windows", "-t", "person", "-F", "#{window_id}"]);
    let mut d = Driver::start(&sb);
    let init = d
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"_meta": {"fleetlayer.dev/node": {
            "protocol": 1, "node": "test", "machine": "test", "user": "test",
            "home": sb.dir.join("home"),
            "config": {"socket": sb.socket, "claude_dir": sb.dir.join("claude"), "session": "agents",
                       "shell": "/bin/sh", "program": sb.tmux}}}}}),
        )
        .unwrap();
    let role = &init["agentCapabilities"]["_meta"]["fleetlayer.dev/plugin"]["roles"]["session_manager"];
    assert_eq!((role["keys"].as_bool(), role["terminal"].as_bool()), (Some(true), Some(true)));

    // Listing: the agent under a shell, named by its --name; the window with no agent isn't a session.
    let planning = d.session("planning");
    let m = meta(&planning);
    assert!(sid(&planning).starts_with("fake-"), "the harness's own ID from its registry: {planning}");
    assert_eq!((m["harness"].as_str(), m["workspace"].as_str()), (Some("claude"), Some("person")));
    assert_eq!((m["humanAttached"].as_bool(), m["opensIn"].as_str()), (Some(false), Some("tmux")));
    assert!(m["address"].as_str().unwrap().starts_with('p'));
    assert!(d.list().iter().all(|s| meta(s)["tab"] != "notes"));
    // A guessed address never reaches a pane without an agent (a shell's screen could hold anything).
    let notes = sb.tmux(&["display-message", "-p", "-t", "person:notes", "#{pane_id}"]);
    let guessed = format!("@p{}", notes.trim().trim_start_matches('%'));
    let e = d.call("_fleetlayer.dev/session/screen", json!({"sessionId": guessed, "lines": 10})).unwrap_err();
    assert_eq!(code(&e), -32010, "{e}");
    d.wait_status("planning", "idle");

    // Delivering: typed in, and the turn that follows is seen working, then done (nobody's looking at it).
    assert_eq!(deliver(&mut d, &planning, "hello there").unwrap()["via"], "typed");
    d.wait_screen(&sid(&planning), "heard: hello there");
    let p = sid(&planning);
    d.wait_note("a working update", |v| {
        v["method"] == "session/update"
            && v["params"]["sessionId"] == p
            && v["params"]["update"]["_meta"]["fleetlayer.dev/session"]["status"] == "working"
    });
    d.wait_status("planning", "done");

    // Nothing is typed while someone scrolls in the pane.
    let pane = format!("%{}", meta(&planning)["address"].as_str().unwrap().trim_start_matches('p'));
    sb.tmux(&["copy-mode", "-t", &pane]);
    let e = deliver(&mut d, &planning, "not now").unwrap_err();
    assert_eq!(code(&e), -32012, "{e}");
    sb.tmux(&["send-keys", "-t", &pane, "-X", "cancel"]);

    // A blocked agent: a message waits for the person, keys answer the prompt.
    let approvals = d.session("approvals");
    assert_eq!(meta(&approvals)["status"], "blocked");
    let e = deliver(&mut d, &approvals, "hello").unwrap_err();
    assert_eq!(code(&e), -32012, "{e}");
    let a = sid(&approvals);
    assert!(d.screen(&a).contains("Do you want to proceed?"));
    d.call("_fleetlayer.dev/session/keys", json!({"sessionId": a, "steps": [{"key": "2"}, {"key": "enter"}]})).unwrap();
    d.wait_screen(&a, "picked: 2");
    d.wait_status("approvals", "done");

    // Watching the terminal through the helper the plugin names.
    let t = d
        .call("_fleetlayer.dev/terminal/command", json!({"sessionId": p, "mode": "observe", "cols": 100, "rows": 30}))
        .unwrap();
    let mut helper = Command::new(t["command"].as_str().unwrap());
    helper.args(t["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()));
    sb.env(&mut helper);
    let mut helper = helper.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut hin = helper.stdin.take().unwrap();
    let mut hout = BufReader::new(helper.stdout.take().unwrap()).lines();
    let mut next = || serde_json::from_str::<Value>(&hout.next().unwrap().unwrap()).unwrap();
    let opened = next();
    assert_eq!((opened["type"].as_str(), opened["width"].as_u64()), (Some("terminal.opened"), Some(120)));
    let frame = next();
    let bytes = base64::engine::general_purpose::STANDARD.decode(frame["bytes"].as_str().unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("heard: hello there"), "{frame}");
    writeln!(hin, "{}", json!({"type": "terminal.paste", "text": "a watcher can't type"})).unwrap();
    writeln!(hin, "{}", json!({"type": "terminal.scrollback", "lines": 50, "id": "s1"})).unwrap();
    let back = loop {
        let v = next();
        if v["type"] == "terminal.scrollback" {
            break v;
        }
    };
    assert_eq!(back["id"], "s1");
    assert!(back["text"].as_str().unwrap().contains("heard: hello there"));
    assert!(!back["text"].as_str().unwrap().contains("a watcher can't type"));
    writeln!(hin, "{}", json!({"type": "terminal.release"})).unwrap();
    let closed = loop {
        let v = next();
        if v["type"] == "terminal.closed" {
            break v;
        }
    };
    assert_eq!(closed["code"], "detached");
    helper.wait().unwrap();

    // Opening: a new window in its own session, ready before the answer, with the first prompt typed in.
    let opened = d
        .call(
            "_fleetlayer.dev/session/open",
            json!({"label": "worker", "cwd": sb.dir.join("work"), "harness": "claude",
                   "args": ["--name", "worker"], "prompt": "first task"}),
        )
        .unwrap();
    assert!(opened["promptError"].is_null(), "{opened}");
    let w = opened["sessionId"].as_str().expect("the registry named it").to_string();
    d.wait_screen(&w, "heard: first task");
    let worker = d.session("worker");
    assert_eq!(meta(&worker)["workspace"], "agents");
    assert_eq!(worker["cwd"].as_str().map(Path::new), Some(sb.dir.join("work").as_path()));

    // Closing it closes its window; the session ends as a closed tab.
    d.call("session/close", json!({"sessionId": w})).unwrap();
    d.wait_note("the worker's end", |v| {
        v["method"] == "_fleetlayer.dev/session/ended"
            && v["params"]["sessionId"] == w
            && v["params"]["how"]["kind"] == "tab_closed"
    });

    // An agent that exits back to its shell ends as agent_exited, and nothing is typed into the shell after.
    deliver(&mut d, &planning, "exit").unwrap();
    d.wait_note("planning's end", |v| {
        v["method"] == "_fleetlayer.dev/session/ended"
            && v["params"]["sessionId"] == p
            && v["params"]["how"]["kind"] == "agent_exited"
    });
    let e = deliver(&mut d, &planning, "echo typed into a shell").unwrap_err();
    assert_eq!(code(&e), -32010, "{e}");

    // The person's own tmux is as it was: no client attached by the plugin, no window added or renamed.
    assert_eq!(sb.tmux(&["list-clients", "-F", "#{client_name}"]), "");
    assert_eq!(sb.tmux(&["list-windows", "-t", "person", "-F", "#{window_id}"]), windows_before);
}
