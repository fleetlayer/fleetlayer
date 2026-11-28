//! The terminal helper: `fleetlayer-rex terminal <observe|input|control> <session> <block> <cols> <rows> <rex>
//! [<server>]`.
//! The node runs it for `fleetlayer attach` (`docs/protocol.md`, "Terminals"), and it speaks the terminal stream
//! format on stdin and stdout.
//!
//! Rex streams a terminal's output over a data connection whose wire format isn't published, so the helper uses the
//! documented block methods instead: it reads the screen with `format` (`vt`, escape sequences that redraw it, with
//! the cursor) a few times a second and sends a full frame when it changed. `terminal.scrollback` is answered from the
//! same `format` answer. In input mode it presses keys (`terminal.keys`, with `write`) and pastes (`terminal.paste`,
//! Rex's `paste` event) and never resizes; control mode also types raw input with `write` and passes resizes to
//! `resize`. In observe mode nothing from the caller reaches Rex. Keys and pastes are confirmed with `terminal.typed`
//! (the stream opens with `typed_replies`) and a failure is answered with `terminal.error`, each carrying the
//! command's `id`, so a caller never takes lost input for typed input.

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::rex::{Failure, Rex};

const POLL: Duration = Duration::from_millis(200);
/// The most scrollback one request reads.
const SCROLLBACK_MAX: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Observe,
    Input,
    Control,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Input => "input",
            Mode::Control => "control",
        }
    }
}

pub struct Args {
    pub mode: Mode,
    pub session: String,
    pub block: String,
    pub cols: u16,
    pub rows: u16,
    pub rex: Rex,
}

pub fn parse_args(a: &[String]) -> Option<Args> {
    let [mode, session, block, cols, rows, program, rest @ ..] = a else { return None };
    let mode = match mode.as_str() {
        "observe" => Mode::Observe,
        "input" => Mode::Input,
        "control" => Mode::Control,
        _ => return None,
    };
    Some(Args {
        mode,
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

/// The last `lines` lines of a `format vt` answer (screen and scrollback), as text without escape sequences, or as
/// they are for `ansi`: `(text, truncated)`.
pub fn scrollback_of(content: &str, lines: usize, ansi: bool) -> (String, bool) {
    let lines = lines.clamp(1, SCROLLBACK_MAX);
    let text = if ansi { content.to_string() } else { strip_escapes(content) };
    let all: Vec<&str> = text.split("\r\n").flat_map(|l| l.split('\n')).collect();
    let mut end = all.len();
    // Blank lines at the end don't count, nor a line holding only escapes (the cursor's position, in `ansi`).
    while end > 0 && strip_escapes(all[end - 1]).trim().is_empty() {
        end -= 1;
    }
    let start = end.saturating_sub(lines);
    (all[start..end].join("\n"), start > 0)
}

/// Text without terminal escape sequences (CSI, OSC, and two-byte escapes) or other control characters but tabs and
/// newlines.
pub fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    // Parameters and intermediates, then one final byte in @..~.
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^') => {
                    // Up to BEL or ST (ESC \\).
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(' | ')' | '*' | '+' | '#') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => out.push('\r'),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Fleetlayer's key names (herdr's, lower case) as the bytes a terminal sends for them. A single character is typed
/// as it is.
pub fn key_bytes(name: &str) -> Option<String> {
    if name.chars().count() == 1 {
        return Some(name.into());
    }
    let lower = name.to_ascii_lowercase();
    let bytes = match lower.as_str() {
        "enter" | "return" => "\r",
        "tab" => "\t",
        "shift+tab" | "btab" | "backtab" => "\u{1b}[Z",
        "backspace" | "bspace" => "\u{7f}",
        "escape" | "esc" => "\u{1b}",
        "space" => " ",
        "up" => "\u{1b}[A",
        "down" => "\u{1b}[B",
        "right" => "\u{1b}[C",
        "left" => "\u{1b}[D",
        "home" => "\u{1b}[H",
        "end" => "\u{1b}[F",
        "pageup" | "pgup" => "\u{1b}[5~",
        "pagedown" | "pgdn" => "\u{1b}[6~",
        "delete" | "del" => "\u{1b}[3~",
        "insert" => "\u{1b}[2~",
        _ => {
            let rest = ["ctrl+", "ctrl-", "c-"].iter().find_map(|p| lower.strip_prefix(p))?;
            let mut chars = rest.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            return c.is_ascii_lowercase().then(|| char::from(c as u8 - b'a' + 1).to_string());
        }
    };
    Some(bytes.into())
}

/// The bytes for a list of key names, or the first name this helper doesn't know.
pub fn keys_bytes(keys: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    for k in keys {
        let name = k.as_str().unwrap_or_default();
        out.push_str(&key_bytes(name).ok_or_else(|| format!("unknown key {name:?}"))?);
    }
    Ok(out)
}

async fn emit(out: &mut tokio::io::Stdout, v: Value) -> std::io::Result<()> {
    out.write_all(format!("{v}\n").as_bytes()).await?;
    out.flush().await
}

async fn vt(a: &Args) -> Result<String, Failure> {
    let v = a
        .rex
        .block(&a.session, &a.block, "format", json!({"format": "vt", "extra_cursor": true, "extra_modes": true}))
        .await?;
    Ok(v.get("content").and_then(Value::as_str).unwrap_or_default().to_string())
}

/// Answer one of the caller's commands that reads or types (scrollback, keys, paste): the scrollback, `terminal.typed`
/// once keys or a paste went in, or `terminal.error`.
async fn answer(a: &Args, v: &Value) -> Option<Value> {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let failed = |message: String| {
        Some(json!({"type": "terminal.error", "code": "command_failed", "message": message, "id": id}))
    };
    let typed = Some(json!({"type": "terminal.typed", "id": id}));
    match v.get("type").and_then(Value::as_str) {
        Some("terminal.scrollback") => {
            let lines = v.get("lines").and_then(Value::as_u64).unwrap_or(200);
            let ansi = v.get("format").and_then(Value::as_str) == Some("ansi");
            match vt(a).await {
                Ok(content) => {
                    let (text, truncated) =
                        scrollback_of(&content, usize::try_from(lines).unwrap_or(SCROLLBACK_MAX), ansi);
                    Some(json!({"type": "terminal.scrollback", "text": text, "truncated": truncated, "id": id}))
                }
                Err(e) => failed(e.message().to_string()),
            }
        }
        Some("terminal.keys") => {
            let keys = v.get("keys").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            let data = match keys_bytes(keys) {
                Ok(d) => d,
                Err(m) => return failed(m),
            };
            if data.is_empty() {
                return typed;
            }
            match a.rex.block(&a.session, &a.block, "write", json!({"data": data})).await {
                Ok(_) => typed,
                Err(e) => failed(format!("pressing keys: {}", e.message())),
            }
        }
        Some("terminal.paste") => {
            let Some(text) = v.get("text").and_then(Value::as_str) else {
                return failed("a paste with no text".into());
            };
            let event = json!({"session_id": a.session, "block_id": a.block, "event": {"type": "paste", "payload": {"text": text}}});
            match a.rex.call("block.event", &event).await {
                Ok(_) => typed,
                Err(e) => failed(format!("pasting: {}", e.message())),
            }
        }
        _ => None,
    }
}

async fn screen(a: &Args) -> Result<(String, u64, u64), Failure> {
    let size = a.rex.block(&a.session, &a.block, "size", json!({})).await?;
    let cols = size.get("columns").and_then(Value::as_u64).unwrap_or(u64::from(a.cols));
    let rows = size.get("rows").and_then(Value::as_u64).unwrap_or(u64::from(a.rows));
    let content = vt(a).await?;
    Ok((frame(&content, usize::try_from(rows).unwrap_or(24)), cols, rows))
}

pub async fn run(a: Args) -> anyhow::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = tokio::io::stdout();
    let control = a.mode == Mode::Control;
    let typing = a.mode != Mode::Observe;
    if control {
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
    emit(
        &mut out,
        json!({"type": "terminal.opened", "mode": a.mode.name(), "width": cols, "height": rows, "typed_replies": true}),
    )
    .await?;
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
                    Some("terminal.input") if control => {
                        let data = match (v.get("text").and_then(Value::as_str), v.get("bytes").and_then(Value::as_str)) {
                            (Some(t), _) => Some(t.to_string()),
                            (None, Some(b)) => b64.decode(b).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
                            _ => None,
                        };
                        if let Some(data) = data {
                            let _ = a.rex.block(&a.session, &a.block, "write", json!({"data": data})).await;
                        }
                    }
                    Some("terminal.resize") if control => {
                        if let (Some(c), Some(r)) = (v.get("cols").and_then(Value::as_u64), v.get("rows").and_then(Value::as_u64)) {
                            let _ = a.rex.block(&a.session, &a.block, "resize", json!({"columns": c, "rows": r})).await;
                        }
                    }
                    Some("terminal.scrollback") => {
                        if let Some(reply) = answer(&a, &v).await {
                            emit(&mut out, reply).await?;
                        }
                    }
                    Some("terminal.keys" | "terminal.paste") if typing => {
                        if let Some(reply) = answer(&a, &v).await {
                            emit(&mut out, reply).await?;
                        }
                    }
                    Some("terminal.keys" | "terminal.paste") => {
                        let id = v.get("id").cloned().unwrap_or(Value::Null);
                        emit(&mut out, json!({"type": "terminal.error", "code": "command_failed", "message": "this terminal was opened to watch, not to type", "id": id})).await?;
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
        assert_eq!(p.mode, Mode::Observe);
        assert_eq!((p.cols, p.rows, p.rex.server), (100, 30, None));
        let mut a = a;
        a[0] = "control".into();
        a.push("ssh://build".into());
        let p = parse_args(&a).unwrap();
        assert_eq!(p.mode, Mode::Control);
        a[0] = "input".into();
        assert_eq!(parse_args(&a).unwrap().mode, Mode::Input);
        assert_eq!(p.rex.server.as_deref(), Some("ssh://build"));
        assert!(parse_args(&["watch".to_string()]).is_none());
    }

    #[test]
    fn scrollback_is_the_last_lines_as_text() {
        let content = "\u{1b}[1mold 1\u{1b}[0m\r\nold 2\r\n\u{1b}]7501;state=idle\u{1b}\\ready> \r\n\r\n\u{1b}[3;8H";
        assert_eq!(scrollback_of(content, 2, false), ("old 2\nready> ".into(), true));
        assert_eq!(scrollback_of(content, 50, false), ("old 1\nold 2\nready> ".into(), false));
        let (ansi, _) = scrollback_of(content, 1, true);
        assert!(ansi.contains("\u{1b}]7501"), "ansi keeps the escapes: {ansi:?}");
    }

    #[test]
    fn key_names_become_terminal_bytes() {
        assert_eq!(key_bytes("enter").as_deref(), Some("\r"));
        assert_eq!(key_bytes("Esc").as_deref(), Some("\u{1b}"));
        assert_eq!(key_bytes("ctrl+c").as_deref(), Some("\u{3}"));
        assert_eq!(key_bytes("down").as_deref(), Some("\u{1b}[B"));
        assert_eq!(key_bytes("y").as_deref(), Some("y"));
        assert_eq!(key_bytes("hyper+q"), None);
        assert_eq!(keys_bytes(&[json!("1"), json!("enter")]), Ok("1\r".into()));
        assert!(keys_bytes(&[json!("enter"), json!("nonsense")]).unwrap_err().contains("nonsense"));
    }
}
