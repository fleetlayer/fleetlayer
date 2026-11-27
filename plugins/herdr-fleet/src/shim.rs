//! The HERDR_BIN shim: this program run as `herdr` (the `bin/herdr` link in the plugin's state directory). A herdr
//! client that runs herdr's CLI gets the merged view through it:
//!
//! - Every command runs the real herdr with `HERDR_SOCKET_PATH` set to the merged socket, so CLI wrappers (`agent
//!   list`, `agent read`, `tab close`…) see every machine.
//! - `terminal session observe|control <target>` streams through Fleetlayer when the target is another machine's
//!   pane, in herdr's own stream format; this machine's panes stream from its herdr as usual. herdr's terminal
//!   streams use a private socket protocol, so they never go through the merged socket.
//! - With the plugin not running (no `shim.json`, or its socket doesn't answer), it's the real herdr, unchanged.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What the plugin writes next to `bin/` for the shim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShimConfig {
    /// The real herdr.
    pub herdr: PathBuf,
    /// The merged socket.
    pub socket: PathBuf,
    /// This machine's herdr socket.
    pub local_socket: PathBuf,
}

/// `<state>/bin/herdr` → `<state>/shim.json`.
pub fn config_path(argv0: &Path) -> Option<PathBuf> {
    Some(argv0.parent()?.parent()?.join("shim.json"))
}

fn read_config(argv0: &Path) -> Option<ShimConfig> {
    serde_json::from_slice(&std::fs::read(config_path(argv0)?).ok()?).ok()
}

/// The first `herdr` on PATH that isn't this program, for when there's no `shim.json`.
fn real_herdr_on_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|d| Path::new(d).join("herdr"))
        .find(|p| p.is_file() && p.canonicalize().ok() != me)
}

/// Replace this process with the real herdr.
fn exec(herdr: &Path, args: &[String], socket: Option<&Path>) -> ! {
    let mut cmd = std::process::Command::new(herdr);
    cmd.args(args);
    match socket {
        Some(s) => cmd.env("HERDR_SOCKET_PATH", s),
        None => cmd.env_remove("HERDR_SOCKET_PATH"),
    };
    let e = cmd.exec();
    eprintln!("herdr (fleetlayer-herdr-fleet shim): can't run {}: {e}", herdr.display());
    std::process::exit(127)
}

/// Ask the merged socket one request.
async fn ask(socket: &Path, method: &str, params: Value) -> Option<Value> {
    let s = tokio::time::timeout(std::time::Duration::from_secs(2), tokio::net::UnixStream::connect(socket))
        .await
        .ok()?
        .ok()?;
    let (r, mut w) = s.into_split();
    let req = json!({"id": "fleetlayer-shim", "method": method, "params": params});
    w.write_all(format!("{req}\n").as_bytes()).await.ok()?;
    let mut lines = BufReader::new(r).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await.ok()?.ok()??;
    serde_json::from_str(&line).ok()
}

/// What `terminal session observe|control <target> [--input] [--cols N] [--rows N] [--takeover]` asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalArgs {
    pub target: String,
    pub mode: &'static str,
    pub cols: u16,
    pub rows: u16,
}

pub fn terminal_args(args: &[String]) -> Option<TerminalArgs> {
    let [t, s, verb, target, rest @ ..] = args else { return None };
    if t != "terminal" || s != "session" || target.starts_with('-') {
        return None;
    }
    let mut a = TerminalArgs {
        target: target.clone(),
        mode: match verb.as_str() {
            "observe" => "observe",
            "control" => "control",
            _ => return None,
        },
        cols: 120,
        rows: 40,
    };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        match flag {
            "--input" if a.mode == "observe" => a.mode = "input",
            "--cols" => a.cols = inline.or_else(|| it.next().cloned())?.parse().ok()?,
            "--rows" => a.rows = inline.or_else(|| it.next().cloned())?.parse().ok()?,
            "--takeover" => {}
            _ => return None,
        }
    }
    Some(a)
}

/// The first record herdr's stream starts with, rewritten to the merged IDs the client knows.
pub fn rewrite_opened(line: &str, pane_id: &str, terminal_id: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(line) else { return line.to_string() };
    if v["type"] != "terminal.opened" {
        return line.to_string();
    }
    v["pane_id"] = json!(pane_id);
    v["terminal_id"] = json!(terminal_id);
    v.to_string()
}

/// Stream another machine's terminal on stdin and stdout, in herdr's format.
async fn stream(resolved: &Value, a: &TerminalArgs) -> i32 {
    let tools = PathBuf::from(resolved["tools_socket"].as_str().unwrap_or_default());
    let address = resolved["address"].as_str().unwrap_or_default();
    let (pane_id, terminal_id) =
        (resolved["pane_id"].as_str().unwrap_or_default(), resolved["terminal_id"].as_str().unwrap_or_default());
    let mut out = tokio::io::stdout();
    let (mut lines, mut w) = match crate::term::open_raw(&tools, address, a.mode, a.cols, a.rows).await {
        Ok(x) => x,
        Err(f) => {
            let closed = json!({"type": "terminal.closed", "code": if f.code == "agent_not_found" { "target_not_found" } else { "error" },
                "reason": f.message});
            let _ = out.write_all(format!("{closed}\n").as_bytes()).await;
            return 1;
        }
    };
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    });
    let mut first = true;
    let mut code = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        let line = if first { rewrite_opened(&line, pane_id, terminal_id) } else { line };
        first = false;
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v["type"] == "terminal.closed"
        {
            code = match v["code"].as_str() {
                Some("pane_closed" | "released" | "taken_over" | "server_restarting" | "server_stopping") => 0,
                _ => 1,
            };
        }
        if out.write_all(format!("{line}\n").as_bytes()).await.is_err() || out.flush().await.is_err() {
            break;
        }
    }
    code
}

/// Run as `herdr`.
pub fn run(argv0: &Path, args: Vec<String>) -> ! {
    let Some(cfg) = read_config(argv0) else {
        match real_herdr_on_path() {
            Some(h) => exec(&h, &args, std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from).as_deref()),
            None => {
                eprintln!("herdr (fleetlayer-herdr-fleet shim): no shim.json next to bin/ and no other herdr on PATH");
                std::process::exit(127)
            }
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    if let Some(a) = terminal_args(&args) {
        let resolved = rt.block_on(ask(&cfg.socket, crate::serve::RESOLVE, json!({"target": a.target})));
        if let Some(r) = resolved.as_ref().map(|v| &v["result"])
            && r["remote"].as_bool() == Some(true)
        {
            let code = rt.block_on(stream(r, &a));
            std::process::exit(code);
        }
        exec(&cfg.herdr, &args, Some(&cfg.local_socket));
    }
    // Is the merged socket up? Without it, the real herdr on its own socket.
    let up = rt.block_on(ask(&cfg.socket, "ping", json!({}))).is_some();
    drop(rt);
    exec(&cfg.herdr, &args, Some(if up { &cfg.socket } else { &cfg.local_socket }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn terminal_commands_are_recognised_with_herdrs_flags() {
        assert_eq!(
            terminal_args(&s(&["terminal", "session", "observe", "w1000:p2", "--input", "--cols", "80", "--rows=24"])),
            Some(TerminalArgs { target: "w1000:p2".into(), mode: "input", cols: 80, rows: 24 })
        );
        assert_eq!(
            terminal_args(&s(&["terminal", "session", "control", "w1:p1", "--takeover"])).map(|a| a.mode),
            Some("control")
        );
        assert_eq!(
            terminal_args(&s(&["terminal", "session", "control", "w1:p1", "--input"])),
            None,
            "control has no --input"
        );
        assert_eq!(terminal_args(&s(&["agent", "list"])), None);
        assert_eq!(terminal_args(&s(&["terminal", "session", "observe", "x", "--unknown"])), None);
    }

    #[test]
    fn the_opened_record_names_the_merged_pane() {
        let line = r#"{"type":"terminal.opened","mode":"observe_input","pane_id":"w1:p6","terminal_id":"term_6f2a91"}"#;
        let v: Value = serde_json::from_str(&rewrite_opened(line, "w1000:p1", "fleetlayer-1000-1")).unwrap();
        assert_eq!((v["pane_id"].as_str(), v["terminal_id"].as_str()), (Some("w1000:p1"), Some("fleetlayer-1000-1")));
        assert_eq!(v["mode"], "observe_input");
        let frame = r#"{"type":"terminal.frame","seq":1}"#;
        assert_eq!(rewrite_opened(frame, "a", "b"), frame);
    }

    #[test]
    fn the_config_sits_beside_the_bin_folder() {
        assert_eq!(config_path(Path::new("/s/herdr-fleet/bin/herdr")), Some(PathBuf::from("/s/herdr-fleet/shim.json")));
    }
}
