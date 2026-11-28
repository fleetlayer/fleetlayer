//! The terminal helper: `fleetlayer-tmux terminal <observe|input|control> <pane> <tmux> [-L <name> | -S <path>]`.
//! The node runs it for `fleetlayer attach` (`docs/protocol.md`, "Terminals"), and it speaks the terminal stream
//! format on stdin and stdout.
//!
//! It never attaches a tmux client, which would count as the person's, change the window's size and show in their
//! status line. It reads the pane with `capture-pane -e` (the screen with its colours) a few times a second and sends
//! a full frame when it changed. Typing goes in with `send-keys` and `paste-buffer`. The pane keeps its size in the
//! person's layout in every mode: a resize from the caller is ignored, and frames say the pane's real size.

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::tmux::{Failure, Socket, Tmux};

const POLL: Duration = Duration::from_millis(150);
/// The most scrollback one request reads.
const SCROLLBACK_MAX: u64 = 10_000;

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
    pub pane: String,
    pub tmux: Tmux,
}

pub fn parse_args(a: &[String]) -> Option<Args> {
    let [mode, pane, program, rest @ ..] = a else { return None };
    let mode = match mode.as_str() {
        "observe" => Mode::Observe,
        "input" => Mode::Input,
        "control" => Mode::Control,
        _ => return None,
    };
    let socket = match rest {
        [] => None,
        [flag, v] if flag == "-L" => Some(Socket::Name(v.clone())),
        [flag, v] if flag == "-S" => Some(Socket::Path(v.clone())),
        _ => return None,
    };
    pane.starts_with('%').then(|| Args { mode, pane: pane.clone(), tmux: Tmux { program: program.clone(), socket } })
}

/// The helper's arguments for a pane, the inverse of `parse_args`.
pub fn helper_args(mode: Mode, pane: &str, tmux: &Tmux) -> Vec<String> {
    let mut a = vec!["terminal".into(), mode.name().into(), pane.into(), tmux.program.clone()];
    a.extend(tmux.socket_args());
    a
}

/// What the pane looks like now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub width: u64,
    pub height: u64,
    pub bytes: String,
}

/// The pane's size and cursor line (`display-message`), then its screen with colours (`capture-pane -e`), read
/// together in one tmux call so they match.
pub const SCREEN_FORMAT: &str = "#{pane_width} #{pane_height} #{cursor_x} #{cursor_y} #{cursor_flag} #{pane_dead}";

/// A frame from what one `display-message \; capture-pane -e` printed: drawn from the top of a cleared screen, each
/// line ending with its attributes reset (tmux starts each captured line from the default style), then the cursor
/// put back where the pane has it.
pub fn frame(out: &str) -> Option<(Screen, bool)> {
    let (head, body) = out.split_once('\n').unwrap_or((out, ""));
    let f: Vec<u64> = head.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    let [width, height, x, y, cursor, dead] = f[..] else { return None };
    let mut lines: Vec<&str> = body.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.truncate(usize::try_from(height).unwrap_or(usize::MAX));
    let mut bytes = String::from("\u{1b}[0m\u{1b}[H\u{1b}[2J");
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            bytes.push_str("\r\n");
        }
        bytes.push_str(l);
        bytes.push_str("\u{1b}[0m");
    }
    bytes.push_str(&format!("\u{1b}[{};{}H", y + 1, x + 1));
    bytes.push_str(if cursor == 1 { "\u{1b}[?25h" } else { "\u{1b}[?25l" });
    Some((Screen { width, height, bytes }, dead == 1))
}

async fn screen(a: &Args) -> Result<(Screen, bool), Failure> {
    let out = a
        .tmux
        .run(&["display-message", "-p", "-t", &a.pane, SCREEN_FORMAT, ";", "capture-pane", "-p", "-e", "-t", &a.pane])
        .await?;
    frame(&out).ok_or_else(|| Failure::Other("tmux printed a screen this helper can't read".into()))
}

/// `lines` lines of the pane's history and screen, as text (or with colours for `format: "ansi"`).
async fn scrollback(a: &Args, lines: u64, ansi: bool) -> Result<(String, bool), Failure> {
    let lines = lines.clamp(1, SCROLLBACK_MAX);
    let have = a.tmux.run(&["display-message", "-p", "-t", &a.pane, "#{history_size} #{pane_height}"]).await?;
    let total: u64 = have.split_whitespace().filter_map(|n| n.parse::<u64>().ok()).sum();
    let start = format!("-{lines}");
    let mut args = vec!["capture-pane", "-p", "-J", "-t", &a.pane, "-S", &start];
    if ansi {
        args.push("-e");
    }
    let text = a.tmux.run(&args).await?;
    let keep: Vec<&str> = text.lines().collect();
    let from = keep.len().saturating_sub(usize::try_from(lines).unwrap_or(usize::MAX));
    Ok((keep[from..].join("\n"), total > lines))
}

async fn emit(out: &mut tokio::io::Stdout, v: Value) -> std::io::Result<()> {
    out.write_all(format!("{v}\n").as_bytes()).await?;
    out.flush().await
}

/// Keys as `terminal.keys` names them (Fleetlayer's names), pressed in order.
async fn press(a: &Args, keys: &[Value]) {
    let steps: Vec<fleetlayer_protocol::ext::KeyStep> =
        keys.iter().filter_map(Value::as_str).map(|k| fleetlayer_protocol::ext::KeyStep::Key(k.into())).collect();
    for step in steps {
        if let Err(e) = a.tmux.keys(&a.pane, std::slice::from_ref(&step)).await {
            tracing::warn!("pressing keys: {}", e.message());
        }
    }
}

pub async fn run(a: Args) -> anyhow::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = tokio::io::stdout();
    let (first, _) = match screen(&a).await {
        Ok(s) => s,
        Err(e) => {
            emit(&mut out, json!({"type": "terminal.error", "message": e.message()})).await?;
            emit(&mut out, json!({"type": "terminal.closed", "code": "not_found", "reason": e.message()})).await?;
            return Ok(());
        }
    };
    emit(
        &mut out,
        json!({"type": "terminal.opened", "mode": a.mode.name(), "width": first.width, "height": first.height}),
    )
    .await?;
    let mut seq = 0u64;
    emit(&mut out, json!({"type": "terminal.frame", "seq": seq, "full": true, "width": first.width, "height": first.height, "encoding": "ansi", "bytes": b64.encode(&first.bytes)})).await?;
    let mut last = first;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut tick = tokio::time::interval(POLL);
    let typing = a.mode != Mode::Observe;
    loop {
        tokio::select! {
            line = input.next_line() => {
                let Ok(Some(line)) = line else { break };
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                match v.get("type").and_then(Value::as_str) {
                    Some("terminal.release") => break,
                    Some("terminal.input") if a.mode == Mode::Control => {
                        let data = match (v.get("text").and_then(Value::as_str), v.get("bytes").and_then(Value::as_str)) {
                            (Some(t), _) => Some(t.as_bytes().to_vec()),
                            (None, Some(b)) => b64.decode(b).ok(),
                            _ => None,
                        };
                        if let Some(data) = data && let Err(e) = a.tmux.send_bytes(&a.pane, &data).await {
                            tracing::warn!("typing: {}", e.message());
                        }
                    }
                    Some("terminal.keys") if typing => {
                        press(&a, v.get("keys").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])).await;
                    }
                    Some("terminal.paste") if typing => {
                        if let Some(t) = v.get("text").and_then(Value::as_str) && let Err(e) = a.tmux.paste(&a.pane, t, true).await {
                            tracing::warn!("pasting: {}", e.message());
                        }
                    }
                    Some("terminal.scrollback") => {
                        let lines = v.get("lines").and_then(Value::as_u64).unwrap_or(200);
                        let ansi = v.get("format").and_then(Value::as_str) == Some("ansi");
                        let id = v.get("id").cloned().unwrap_or(Value::Null);
                        let reply = match scrollback(&a, lines, ansi).await {
                            Ok((text, truncated)) => json!({"type": "terminal.scrollback", "text": text, "truncated": truncated, "id": id}),
                            Err(e) => json!({"type": "terminal.error", "message": e.message(), "id": id}),
                        };
                        emit(&mut out, reply).await?;
                    }
                    // The pane belongs to the person's layout: it keeps its size.
                    _ => {}
                }
            }
            _ = tick.tick() => {
                match screen(&a).await {
                    Ok((_, true)) => {
                        emit(&mut out, json!({"type": "terminal.closed", "code": "closed", "reason": "the program in the pane exited"})).await?;
                        return Ok(());
                    }
                    Ok((now, false)) => {
                        if now != last {
                            seq += 1;
                            emit(&mut out, json!({"type": "terminal.frame", "seq": seq, "full": true, "width": now.width, "height": now.height, "encoding": "ansi", "bytes": b64.encode(&now.bytes)})).await?;
                            last = now;
                        }
                    }
                    Err(Failure::NotHere(m)) => {
                        emit(&mut out, json!({"type": "terminal.closed", "code": "closed", "reason": m})).await?;
                        return Ok(());
                    }
                    Err(Failure::NoServer) => {
                        emit(&mut out, json!({"type": "terminal.closed", "code": "closed", "reason": "the tmux server stopped"})).await?;
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

/// The helper for a session's pane, as the plugin names it to the node.
pub fn command(
    tmux: &Tmux,
    pane: &str,
    mode: fleetlayer_protocol::ext::TerminalMode,
) -> fleetlayer_protocol::ext::TerminalCommand {
    let helper = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "fleetlayer-tmux".into());
    let mode = match mode {
        fleetlayer_protocol::ext::TerminalMode::Observe => Mode::Observe,
        fleetlayer_protocol::ext::TerminalMode::Input => Mode::Input,
        fleetlayer_protocol::ext::TerminalMode::Control => Mode::Control,
    };
    fleetlayer_protocol::ext::TerminalCommand {
        command: helper,
        args: helper_args(mode, pane, tmux),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_redraws_the_pane_and_puts_the_cursor_back() {
        let out = "80 3 4 1 1 0\nfirst\n\u{1b}[31mred\nthird\n";
        let (s, dead) = frame(out).unwrap();
        assert!(!dead);
        assert_eq!((s.width, s.height), (80, 3));
        assert_eq!(
            s.bytes,
            "\u{1b}[0m\u{1b}[H\u{1b}[2Jfirst\u{1b}[0m\r\n\u{1b}[31mred\u{1b}[0m\r\nthird\u{1b}[0m\u{1b}[2;5H\u{1b}[?25h"
        );
        let (s, dead) = frame("80 1 0 0 0 1\nbye\n").unwrap();
        assert!(dead);
        assert!(s.bytes.ends_with("\u{1b}[?25l"), "a hidden cursor stays hidden");
        assert!(frame("not a header\n").is_none());
    }

    #[test]
    fn helper_arguments_round_trip() {
        let t = Tmux { program: "/usr/bin/tmux".into(), socket: Some(Socket::Name("work".into())) };
        let a = helper_args(Mode::Input, "%7", &t);
        assert_eq!(a, ["terminal", "input", "%7", "/usr/bin/tmux", "-L", "work"]);
        let p = parse_args(&a[1..]).unwrap();
        assert_eq!((p.mode, p.pane.as_str(), p.tmux.socket), (Mode::Input, "%7", Some(Socket::Name("work".into()))));
        let s: Vec<String> = ["observe", "%1", "tmux", "-S", "/run/t.sock"].map(String::from).into();
        assert_eq!(parse_args(&s).unwrap().tmux.socket, Some(Socket::Path("/run/t.sock".into())));
        assert!(parse_args(&["watch".to_string(), "%1".into(), "tmux".into()]).is_none());
        assert!(parse_args(&["observe".to_string(), "1".into(), "tmux".into()]).is_none(), "a pane ID only");
        assert!(parse_args(&["observe".to_string(), "%1".into(), "tmux".into(), "-X".into(), "y".into()]).is_none());
    }
}
