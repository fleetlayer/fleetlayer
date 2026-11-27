//! The terminal helper: `fleetlayer-rex terminal <observe|control> <session> <block> <cols> <rows> <rex> [<server>]`.
//! The node runs it for `fleetlayer attach` (`docs/protocol.md`, "Terminals"), and it speaks the terminal stream
//! format on stdin and stdout.
//!
//! Rex streams a terminal's output over a data connection whose wire format isn't published, so the helper uses the
//! documented block methods instead: it reads the screen with `format` (`vt`, escape sequences that redraw it, with
//! the cursor) a few times a second and sends a full frame when it changed, and in control mode it types input with
//! `write` and passes resizes to `resize`. In observe mode nothing from the caller reaches Rex.

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::rex::{Failure, Rex};

const POLL: Duration = Duration::from_millis(200);

pub struct Args {
    pub control: bool,
    pub session: String,
    pub block: String,
    pub cols: u16,
    pub rows: u16,
    pub rex: Rex,
}

pub fn parse_args(a: &[String]) -> Option<Args> {
    let [mode, session, block, cols, rows, program, rest @ ..] = a else { return None };
    let control = match mode.as_str() {
        "control" => true,
        "observe" => false,
        _ => return None,
    };
    Some(Args {
        control,
        session: session.clone(),
        block: block.clone(),
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        rex: Rex { program: program.clone(), server: rest.first().cloned() },
    })
}

/// The last `rows` lines of a `format vt` answer (it holds the scrollback too), drawn from the top of a cleared screen.
/// What follows the last line (the cursor position) is kept.
pub fn frame(content: &str, rows: usize) -> String {
    let lines: Vec<&str> = content.split("\r\n").collect();
    let start = lines.len().saturating_sub(rows.max(1));
    format!("\u{1b}[H\u{1b}[2J{}", lines[start..].join("\r\n"))
}

async fn emit(out: &mut tokio::io::Stdout, v: Value) -> std::io::Result<()> {
    out.write_all(format!("{v}\n").as_bytes()).await?;
    out.flush().await
}

async fn screen(a: &Args) -> Result<(String, u64, u64), Failure> {
    let size = a.rex.block(&a.session, &a.block, "size", json!({})).await?;
    let cols = size.get("columns").and_then(Value::as_u64).unwrap_or(u64::from(a.cols));
    let rows = size.get("rows").and_then(Value::as_u64).unwrap_or(u64::from(a.rows));
    let v = a
        .rex
        .block(&a.session, &a.block, "format", json!({"format": "vt", "extra_cursor": true, "extra_modes": true}))
        .await?;
    let content = v.get("content").and_then(Value::as_str).unwrap_or_default();
    Ok((frame(content, usize::try_from(rows).unwrap_or(24)), cols, rows))
}

pub async fn run(a: Args) -> anyhow::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = tokio::io::stdout();
    if a.control {
        // Ask for the caller's size; Rex applies it only when nobody else owns the terminal.
        let _ = a.rex.block(&a.session, &a.block, "resize", json!({"columns": a.cols, "rows": a.rows})).await;
    }
    let (first, cols, rows) = match screen(&a).await {
        Ok(s) => s,
        Err(e) => {
            emit(&mut out, json!({"type": "terminal.error", "message": e.message()})).await?;
            emit(&mut out, json!({"type": "terminal.closed", "code": "not_found", "reason": e.message()})).await?;
            return Ok(());
        }
    };
    emit(&mut out, json!({"type": "terminal.opened", "mode": if a.control { "control" } else { "observe" }, "width": cols, "height": rows})).await?;
    let mut seq = 0u64;
    emit(&mut out, json!({"type": "terminal.frame", "seq": seq, "full": true, "width": cols, "height": rows, "encoding": "ansi", "bytes": b64.encode(&first)})).await?;
    let mut last = first;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut tick = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            line = input.next_line() => {
                let Ok(Some(line)) = line else { break };
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                match v.get("type").and_then(Value::as_str) {
                    Some("terminal.release") => break,
                    Some("terminal.input") if a.control => {
                        let data = match (v.get("text").and_then(Value::as_str), v.get("bytes").and_then(Value::as_str)) {
                            (Some(t), _) => Some(t.to_string()),
                            (None, Some(b)) => b64.decode(b).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
                            _ => None,
                        };
                        if let Some(data) = data {
                            let _ = a.rex.block(&a.session, &a.block, "write", json!({"data": data})).await;
                        }
                    }
                    Some("terminal.resize") if a.control => {
                        if let (Some(c), Some(r)) = (v.get("cols").and_then(Value::as_u64), v.get("rows").and_then(Value::as_u64)) {
                            let _ = a.rex.block(&a.session, &a.block, "resize", json!({"columns": c, "rows": r})).await;
                        }
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                match screen(&a).await {
                    Ok((now, cols, rows)) => {
                        if now != last {
                            seq += 1;
                            emit(&mut out, json!({"type": "terminal.frame", "seq": seq, "full": true, "width": cols, "height": rows, "encoding": "ansi", "bytes": b64.encode(&now)})).await?;
                            last = now;
                        }
                    }
                    Err(Failure::NotHere(m)) => {
                        emit(&mut out, json!({"type": "terminal.closed", "code": "closed", "reason": m})).await?;
                        return Ok(());
                    }
                    Err(Failure::Other(m)) => tracing::warn!("reading the screen: {m}"),
                }
            }
        }
    }
    emit(&mut out, json!({"type": "terminal.closed", "code": "detached", "reason": "detached"})).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_the_screen_without_the_scrollback() {
        // As `format vt` with extra_cursor answered (rex 0.1.0-preview.25), with two lines of scrollback.
        let content = "old 1\r\nold 2\r\nagent working\r\nyes\r\ngot: yes\u{1b}[3;9H";
        assert_eq!(frame(content, 3), "\u{1b}[H\u{1b}[2Jagent working\r\nyes\r\ngot: yes\u{1b}[3;9H");
        assert_eq!(frame("one line", 24), "\u{1b}[H\u{1b}[2Jone line");
    }

    #[test]
    fn helper_arguments_parse() {
        let a: Vec<String> = ["observe", "session:a", "block:b", "100", "30", "/usr/bin/rex"].map(String::from).into();
        let p = parse_args(&a).unwrap();
        assert!(!p.control);
        assert_eq!((p.cols, p.rows, p.rex.server), (100, 30, None));
        let mut a = a;
        a[0] = "control".into();
        a.push("ssh://build".into());
        let p = parse_args(&a).unwrap();
        assert!(p.control);
        assert_eq!(p.rex.server.as_deref(), Some("ssh://build"));
        assert!(parse_args(&["watch".to_string()]).is_none());
    }
}
