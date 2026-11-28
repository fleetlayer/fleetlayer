//! `fleetlayer commands` and `fleetlayer run`: the commands other nodes share (`docs/guides/shared-commands.md`), and
//! running one. A run goes through this node's tools socket to a stream the control plane pairs with the sharing node,
//! which checks every argument, asks the control plane about the run, and streams the program's output back.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Exit codes of our own, apart from the program's: refused or couldn't run (125, as `docker run` and `git bisect`
/// use it), stopped at its timeout (124, as `timeout` does), cancelled (130, as Ctrl-C).
pub const REFUSED: i32 = 125;
pub const TIMED_OUT: i32 = 124;
pub const CANCELLED: i32 = 130;

/// The commands in `node/me`'s answer, optionally only one node's.
pub fn listed<'a>(me: &'a Value, node: Option<&str>) -> Vec<&'a Value> {
    me["me"]["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| node.is_none_or(|n| c["node"].as_str() == Some(n)))
        .collect()
}

/// `fleetlayer commands` for a person or an agent to read.
pub fn commands_text(me: &Value, node: Option<&str>) -> String {
    use std::fmt::Write;
    let list = listed(me, node);
    if me["offline"] == true {
        return format!("{} isn't linked to a control plane now, so it can't say.", me["node"].as_str().unwrap_or("?"));
    }
    if list.is_empty() {
        return match node {
            Some(n) => format!("{n} shares no commands you may run."),
            None => "No node shares a command you may run.".into(),
        };
    }
    let mut o = String::new();
    for c in list {
        let _ = writeln!(o, "{}{}", c["name"].as_str().unwrap_or("?"), state_note(c));
        if let Some(d) = c["description"].as_str() {
            let _ = writeln!(o, "    {d}");
        }
        let _ = writeln!(o, "    fleetlayer run {} {}", c["name"].as_str().unwrap_or("?"), usage(c));
        for p in c["params"].as_array().into_iter().flatten() {
            let mut line = format!("    --{}: {}", p["name"].as_str().unwrap_or("?"), param_kind(p));
            if let Some(d) = p["description"].as_str() {
                line.push_str(&format!(". {d}"));
            }
            let _ = writeln!(o, "{line}");
        }
        if let Some(t) = c["timeout"].as_u64().filter(|t| *t > 0) {
            let _ = writeln!(o, "    Stopped after {}.", duration(t));
        }
    }
    o.trim_end().to_string()
}

fn state_note(c: &Value) -> String {
    let mut s = match c["state"].as_str() {
        Some("ask") => " (each run asks your person first)".to_string(),
        Some("pending") => format!(" (waiting for your person: request {})", c["request"].as_str().unwrap_or("?")),
        Some("refused") => format!(" (your person refused request {})", c["request"].as_str().unwrap_or("?")),
        _ => String::new(),
    };
    if c["offline"] == true {
        s.push_str(" (its node is offline)");
    }
    s
}

fn duration(secs: u64) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// What a parameter takes, in words.
fn param_kind(p: &Value) -> String {
    let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
    let mut out = match p["type"].as_str().unwrap_or("?") {
        "choice" => format!(
            "one of {}",
            p["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
        ),
        "text" => format!("text, at most {} characters, not starting with -", p["maxLength"].as_u64().unwrap_or(0)),
        "pattern" => format!("matching {}", s("pattern")),
        "file" => {
            let ext = p["extension"].as_str().map(|e| format!("a {e} file")).unwrap_or_else(|| "a file".into());
            format!("{ext} here, at most {} bytes (its path)", p["maxBytes"].as_u64().unwrap_or(0))
        }
        other => other.to_string(),
    };
    if let Some(d) = p["default"].as_str() {
        out.push_str(&format!(" (default {d})"));
    }
    out
}

/// `--rack <rack> --script <file>`, optional parameters in brackets.
fn usage(c: &Value) -> String {
    c["params"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let n = p["name"].as_str().unwrap_or("?");
            let v = if p["type"] == "file" { "file" } else { n };
            if p["default"].is_string() { format!("[--{n} <{v}>]") } else { format!("--{n} <{v}>") }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The arguments after the command: `--name value` or `--name=value`, and `--why` for the run's reason.
pub fn parse_args(args: &[String]) -> anyhow::Result<(BTreeMap<String, String>, Option<String>)> {
    let mut out = BTreeMap::new();
    let mut why = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let Some(flag) = a.strip_prefix("--") else {
            anyhow::bail!("{a:?}: give each parameter as --<name> <value>");
        };
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => {
                let v = it.next().ok_or_else(|| anyhow::anyhow!("--{flag} needs a value"))?;
                (flag.to_string(), v.clone())
            }
        };
        if name.is_empty() {
            anyhow::bail!("{a:?}: give each parameter as --<name> <value>");
        }
        if name == "why" {
            why = Some(value);
            continue;
        }
        if out.insert(name.clone(), value).is_some() {
            anyhow::bail!("--{name} is given twice");
        }
    }
    Ok((out, why))
}

/// The run request: plain values as they are, file parameters read from this machine with their SHA-256.
pub fn request(command: &Value, args: BTreeMap<String, String>, cwd: &Path) -> anyhow::Result<Value> {
    let params: Vec<&Value> = command["params"].as_array().into_iter().flatten().collect();
    let name = command["name"].as_str().unwrap_or("?");
    let mut plain = serde_json::Map::new();
    let mut files = serde_json::Map::new();
    for (k, v) in args {
        let Some(p) = params.iter().find(|p| p["name"].as_str() == Some(k.as_str())) else {
            anyhow::bail!("{name} takes no --{k}: fleetlayer run {name} {}", usage(command));
        };
        if p["type"] != "file" {
            plain.insert(k, json!(v));
            continue;
        }
        let path = cwd.join(&v);
        let max = p["maxBytes"].as_u64().unwrap_or(u64::MAX);
        let size = std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("--{k} {}: {e}", path.display()))?.len();
        if size > max {
            anyhow::bail!("--{k} {} is {size} bytes; {name} takes at most {max}", path.display());
        }
        let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("--{k} {}: {e}", path.display()))?;
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        files.insert(
            k,
            json!({
                "name": file_name,
                "sha256": hex::encode(sha2::Sha256::digest(&bytes)),
                "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            }),
        );
    }
    for p in &params {
        let n = p["name"].as_str().unwrap_or("?");
        if !p["default"].is_string() && !plain.contains_key(n) && !files.contains_key(n) {
            anyhow::bail!("{name} needs --{n}: fleetlayer run {name} {}", usage(command));
        }
    }
    Ok(json!({"type": "run", "args": plain, "files": files}))
}

/// Run it: the program's stdout and stderr come out here as they arrive; the answer is the exit code to leave with.
/// Ctrl-C asks the other node to stop it (a second one leaves at once). With `raw`, the stream's JSON lines are printed
/// as they come.
pub async fn run(
    tools_socket: &Path,
    target: &str,
    why: Option<&str>,
    req: Value,
    raw: bool,
    caller: Value,
) -> anyhow::Result<i32> {
    let stream = tokio::net::UnixStream::connect(tools_socket)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach the node's tools socket {} ({e})", tools_socket.display()))?;
    let (r, mut w) = stream.into_split();
    let open = json!({"command": target, "why": why, "sessionId": caller["sessionId"], "address": caller["address"]});
    w.write_all(format!("{open}\n").as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let first = lines.next_line().await?.unwrap_or_default();
    let answer: Value = serde_json::from_str(&first).unwrap_or_default();
    if let Some(e) = answer["error"].as_str() {
        eprintln!("fleetlayer: {e}");
        return Ok(REFUSED);
    }
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut out = tokio::io::stdout();
    let mut err = tokio::io::stderr();
    let mut interrupts = 0;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    eprintln!("fleetlayer: the connection to {target} ended before the command did");
                    return Ok(REFUSED);
                };
                if raw {
                    out.write_all(format!("{line}\n").as_bytes()).await?;
                    out.flush().await?;
                }
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                match v["type"].as_str() {
                    Some("output") if !raw => {
                        let text = v["text"].as_str().unwrap_or_default().as_bytes();
                        if v["stream"] == "stderr" {
                            err.write_all(text).await?;
                            err.flush().await?;
                        } else {
                            out.write_all(text).await?;
                            out.flush().await?;
                        }
                    }
                    Some("waiting") if !raw => eprintln!(
                        "fleetlayer: waiting for your person to approve request {} ({})",
                        v["approval"].as_str().unwrap_or("?"),
                        v["reason"].as_str().unwrap_or("")
                    ),
                    Some("note") if !raw => eprintln!("fleetlayer: {}", v["text"].as_str().unwrap_or("")),
                    Some("refused") => {
                        if !raw {
                            eprintln!("fleetlayer: {}", v["reason"].as_str().unwrap_or("refused"));
                        }
                        return Ok(REFUSED);
                    }
                    Some("exit") => {
                        if let Some(c) = v["code"].as_i64() {
                            return Ok(c as i32);
                        }
                        if !raw {
                            eprintln!("fleetlayer: {target} {}", v["reason"].as_str().unwrap_or("was stopped"));
                        }
                        return Ok(match v["stopped"].as_str() {
                            Some("timeout") => TIMED_OUT,
                            Some("cancelled") => CANCELLED,
                            _ => 128 + v["signal"].as_i64().unwrap_or(1) as i32,
                        });
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                interrupts += 1;
                if interrupts > 1 {
                    return Ok(CANCELLED);
                }
                eprintln!("fleetlayer: stopping {target} (Ctrl-C again to leave without waiting)");
                let _ = w.write_all(b"{\"type\":\"cancel\"}\n").await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> Value {
        json!({"name": "rack-gateway-run@laptop", "node": "laptop", "state": "ask", "timeout": 1800,
        "description": "Run a script on a rack",
        "params": [
            {"name": "rack", "type": "choice", "values": ["staging", "production"]},
            {"name": "script", "type": "file", "maxBytes": 16, "extension": ".rb"},
            {"name": "reason", "type": "text", "maxLength": 200},
            {"name": "n", "type": "pattern", "pattern": "[0-9]+", "maxLength": 200, "default": "1"},
        ]})
    }

    #[test]
    fn arguments_are_flags_with_values_and_why_is_ours() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let (args, why) = a(&["--rack", "staging", "--reason=fix it", "--why", "because"]).unwrap();
        assert_eq!(args.get("rack").map(String::as_str), Some("staging"));
        assert_eq!(args.get("reason").map(String::as_str), Some("fix it"));
        assert_eq!(why.as_deref(), Some("because"));
        // A value may start with -: the sharing node decides whether it may.
        assert_eq!(a(&["--reason", "-x"]).unwrap().0["reason"], "-x");
        assert!(a(&["staging"]).is_err());
        assert!(a(&["--rack"]).is_err());
        assert!(a(&["--rack", "a", "--rack", "b"]).is_err());
    }

    #[test]
    fn files_are_read_here_and_sent_with_their_hash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fix.rb"), b"puts 1\n").unwrap();
        let args = BTreeMap::from([
            ("rack".to_string(), "staging".to_string()),
            ("script".to_string(), "fix.rb".to_string()),
            ("reason".to_string(), "x".to_string()),
        ]);
        let r = request(&command(), args.clone(), tmp.path()).unwrap();
        assert_eq!(r["args"], json!({"rack": "staging", "reason": "x"}));
        assert_eq!(r["files"]["script"]["name"], "fix.rb");
        assert_eq!(r["files"]["script"]["sha256"], hex::encode(sha2::Sha256::digest(b"puts 1\n")));
        assert_eq!(r["files"]["script"]["base64"], "cHV0cyAxCg==");
        // Too big is refused before anything is sent.
        std::fs::write(tmp.path().join("big.rb"), [b'a'; 17]).unwrap();
        let mut big = args.clone();
        big.insert("script".into(), "big.rb".into());
        assert!(request(&command(), big, tmp.path()).unwrap_err().to_string().contains("at most 16"));
        let mut missing = args.clone();
        missing.remove("rack");
        assert!(request(&command(), missing, tmp.path()).unwrap_err().to_string().contains("needs --rack"));
        let mut extra = args;
        extra.insert("other".into(), "1".into());
        assert!(request(&command(), extra, tmp.path()).unwrap_err().to_string().contains("takes no --other"));
    }

    #[test]
    fn the_list_says_how_to_run_each_command() {
        let me = json!({"node": "agents", "me": {"commands": [command()]}});
        let t = commands_text(&me, None);
        assert!(t.starts_with("rack-gateway-run@laptop (each run asks your person first)"), "{t}");
        assert!(
            t.contains(
                "fleetlayer run rack-gateway-run@laptop --rack <rack> --script <file> --reason <reason> [--n <n>]"
            ),
            "{t}"
        );
        assert!(t.contains("--rack: one of staging, production"), "{t}");
        assert!(t.contains("--script: a .rb file here, at most 16 bytes"), "{t}");
        assert!(t.contains("Stopped after 30m."), "{t}");
        assert_eq!(commands_text(&me, Some("other")), "other shares no commands you may run.");
    }
}
