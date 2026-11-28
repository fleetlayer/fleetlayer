//! tmux, in tmux's own terms: its command line (`tmux list-panes -a -F …`, `capture-pane`, `send-keys`,
//! `paste-buffer`, `new-window -d`), argv only, never a shell. Nothing here attaches a client, so the person's own
//! clients, sizes and layouts stay exactly as they were.
//!
//! tmux's model: a server holds sessions, a session holds windows, a window holds panes. Fleetlayer's session is one
//! agent running in one pane, found by walking the processes under the pane's own process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fleetlayer_protocol::ext::{KeyStep, Status};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

/// Agent programs this plugin recognises in a pane, by the program's name. Anything else (a shell, an editor) isn't
/// a session.
pub const HARNESSES: &[&str] =
    &["claude", "codex", "opencode", "pi", "gemini", "crush", "amp", "cursor-agent", "goose", "aider", "droid"];

/// Session managers of their own: an agent under one of these in a pane (herdr run inside tmux, say) is theirs to
/// list, not this plugin's, or the fleet would see it twice.
const OTHER_MANAGERS: &[&str] = &["herdr", "rex", "zellij", "screen", "tmux"];

/// Which tmux server: the person's default one, or a named socket (`-L`) or a socket path (`-S`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    Name(String),
    Path(String),
}

/// How to run `tmux`.
#[derive(Debug, Clone)]
pub struct Tmux {
    pub program: String,
    pub socket: Option<Socket>,
}

/// What a call can fail with.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// No tmux server is running (or none at that socket): nothing to list, nothing to act on.
    NoServer,
    /// The pane, window or session isn't there.
    NotHere(String),
    /// Anything else, as a sentence.
    Other(String),
}

impl Failure {
    pub fn message(&self) -> String {
        match self {
            Failure::NoServer => "no tmux server is running".into(),
            Failure::NotHere(m) | Failure::Other(m) => m.clone(),
        }
    }
}

/// A pane's address in the fleet: its tmux pane ID without the `%` (`p12` for `%12`), which would need escaping in
/// links and URLs.
pub fn address(pane_id: &str) -> String {
    format!("p{}", pane_id.trim_start_matches('%'))
}

/// The tmux pane ID of an address (`%12` for `p12`), or None for anything else.
pub fn pane_id(address: &str) -> Option<String> {
    let n = address.strip_prefix('p')?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("%{n}"))
}

/// Why a program couldn't be run, as a sentence: a missing program says how to point the plugin at it.
pub fn spawn_error(program: &str, e: &std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("running {program}: {e}");
    }
    let fix = "give the tmux plugin `config: { program: <full path to tmux> }` in node.yaml";
    if program.contains('/') {
        format!("there's no tmux at {program}: {fix}")
    } else {
        let path = std::env::var("PATH").unwrap_or_default();
        format!("{program} isn't on the node's PATH ({path}): {fix}")
    }
}

/// tmux's error, sorted into a kind.
pub fn classify(stderr: &str) -> Failure {
    let line = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("tmux failed").to_string();
    if line.starts_with("no server running") || line.starts_with("error connecting to") {
        return Failure::NoServer;
    }
    if line.starts_with("can't find") || line.starts_with("no such") {
        return Failure::NotHere(line);
    }
    Failure::Other(line)
}

impl Tmux {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.program);
        match &self.socket {
            Some(Socket::Name(n)) => {
                c.args(["-L", n]);
            }
            Some(Socket::Path(p)) => {
                c.args(["-S", p]);
            }
            None => {}
        }
        c.stdin(std::process::Stdio::null()).kill_on_drop(true);
        c
    }

    /// The arguments that pick the server, for a helper that runs tmux itself.
    pub fn socket_args(&self) -> Vec<String> {
        match &self.socket {
            Some(Socket::Name(n)) => vec!["-L".into(), n.clone()],
            Some(Socket::Path(p)) => vec!["-S".into(), p.clone()],
            None => vec![],
        }
    }

    /// Run tmux with `args` (and `input` on its stdin), answering what it printed.
    pub async fn run_with(&self, args: &[&str], input: Option<&[u8]>) -> Result<String, Failure> {
        let mut c = self.command();
        c.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        if input.is_some() {
            c.stdin(std::process::Stdio::piped());
        }
        let mut child = c.spawn().map_err(|e| Failure::Other(spawn_error(&self.program, &e)))?;
        if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(data).await.map_err(|e| Failure::Other(format!("writing to tmux: {e}")))?;
        }
        let what = args.first().copied().unwrap_or("tmux");
        let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .map_err(|_| Failure::Other(format!("tmux {what} didn't answer within 10 seconds")))?
            .map_err(|e| Failure::Other(format!("tmux {what}: {e}")))?;
        if !out.status.success() {
            return Err(classify(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub async fn run(&self, args: &[&str]) -> Result<String, Failure> {
        self.run_with(args, None).await
    }

    /// Every pane on the server; none when no server is running.
    pub async fn panes(&self) -> Result<Vec<Pane>, Failure> {
        match self.run(&["list-panes", "-a", "-F", PANE_FORMAT]).await {
            Ok(out) => Ok(out.lines().filter_map(parse_pane).collect()),
            Err(Failure::NoServer) => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    /// One pane, or NotHere.
    pub async fn pane(&self, pane: &str) -> Result<Pane, Failure> {
        let out = self.run(&["display-message", "-p", "-t", pane, "-F", PANE_FORMAT]).await?;
        out.lines().find_map(parse_pane).ok_or_else(|| Failure::NotHere(format!("can't find pane {pane}")))
    }

    /// Every agent in a pane on the server, with its status, and the IDs of every pane (so a caller can tell an agent
    /// that exited back to a shell from a pane that closed).
    pub async fn snapshot(&self, claude_sessions: &Path) -> Result<(Vec<Agent>, Vec<String>), Failure> {
        let panes = self.panes().await?;
        if panes.is_empty() {
            return Ok((vec![], vec![]));
        }
        let procs = processes().await;
        let regs = claude_registrations(claude_sessions).await;
        let mut agents = vec![];
        for p in panes.iter().filter(|p| !p.dead) {
            let Some(proc_) = find_agent(p.pid, &procs) else { continue };
            let harness = harness_of(proc_).unwrap_or_default();
            let reg = (harness == "claude").then(|| regs.iter().find(|r| r.pid == proc_.pid)).flatten();
            let mut status = reg.and_then(|r| r.status.as_deref()).map(registry_status);
            if status.is_none() {
                let screen = self.screen(&p.id, 30).await.unwrap_or_default();
                status = Some(screen_status(&harness, &screen));
            }
            let session_id = match reg {
                Some(r) => Some(r.session_id.clone()),
                None if harness == "codex" => codex_session(proc_.pid),
                None => None,
            };
            agents.push(Agent {
                pane: p.clone(),
                pid: proc_.pid,
                harness,
                session_id,
                status: status.unwrap_or_default(),
                claude_name: reg.filter(|r| r.name_source.as_deref() == Some("user")).and_then(|r| r.name.clone()),
                tab: String::new(),
            });
        }
        assign_tabs(&mut agents);
        Ok((agents, panes.into_iter().map(|p| p.id).collect()))
    }

    /// What a pane shows now, as plain text: the last `lines` lines of its visible screen.
    pub async fn screen(&self, pane: &str, lines: usize) -> Result<String, Failure> {
        let out = self.run(&["capture-pane", "-p", "-t", pane]).await?;
        Ok(last_lines(&out, lines))
    }

    /// Type text into a pane as one paste, through a buffer of its own (never the person's buffers), so nothing in
    /// the text is read as a tmux command or a key name. `bracketed` wraps it in the terminal's paste markers when the
    /// program asked for them, so newlines stay inside one message.
    pub async fn paste(&self, pane: &str, text: &str, bracketed: bool) -> Result<(), Failure> {
        let buffer = format!("fleetlayer-{}-{}", std::process::id(), next_buffer());
        self.run_with(&["load-buffer", "-b", &buffer, "-"], Some(text.as_bytes())).await?;
        // -r: newlines stay newlines (tmux would otherwise send each as Enter); -d: the buffer goes once pasted.
        let mut args = vec!["paste-buffer", "-d", "-r", "-b", &buffer, "-t", pane];
        if bracketed {
            args.push("-p");
        }
        let r = self.run(&args).await.map(|_| ());
        if r.is_err() {
            let _ = self.run(&["delete-buffer", "-b", &buffer]).await;
        }
        r
    }

    /// Press named keys (`enter`, `tab`, `backspace`, `escape`, arrows, `ctrl+c`) in a pane.
    pub async fn press(&self, pane: &str, names: &[String]) -> Result<(), Failure> {
        let mut args: Vec<&str> = vec!["send-keys", "-t", pane];
        args.extend(names.iter().map(String::as_str));
        self.run(&args).await.map(|_| ())
    }

    /// Carry out key steps as Fleetlayer sends them: a `key` step is one or more space-separated key names or single
    /// characters, a `text` step is typed as it is.
    pub async fn keys(&self, pane: &str, steps: &[KeyStep]) -> Result<(), Failure> {
        for (i, step) in steps.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            match step {
                KeyStep::Text(t) => self.paste(pane, t, false).await?,
                KeyStep::Key(k) => {
                    for token in k.split_whitespace() {
                        match key(token) {
                            Some(Key::Named(n)) => self.press(pane, &[n]).await?,
                            Some(Key::Literal(c)) => self.paste(pane, &c, false).await?,
                            None => return Err(Failure::Other(format!("{token} isn't a key tmux can press"))),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Send raw bytes to a pane, exactly as typed on a terminal.
    pub async fn send_bytes(&self, pane: &str, bytes: &[u8]) -> Result<(), Failure> {
        if bytes.is_empty() {
            return Ok(());
        }
        for chunk in bytes.chunks(512) {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let mut args: Vec<&str> = vec!["send-keys", "-t", pane, "-H"];
            args.extend(hex.iter().map(String::as_str));
            self.run(&args).await?;
        }
        Ok(())
    }

    /// Type a message into an agent and submit it: a bracketed paste, then Enter once the agent has taken it in.
    pub async fn type_message(&self, pane: &str, text: &str) -> Result<(), Failure> {
        self.paste(pane, text, true).await?;
        // The agent takes the paste in before the Enter, or the Enter lands inside the pasted text.
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.press(pane, &["Enter".into()]).await
    }

    /// Press Esc in a pane: an agent stops its turn.
    pub async fn interrupt(&self, pane: &str) -> Result<(), Failure> {
        self.press(pane, &["Escape".into()]).await
    }

    /// Close the pane (and its window, when it's the window's last pane).
    pub async fn kill_pane(&self, pane: &str) -> Result<(), Failure> {
        self.run(&["kill-pane", "-t", pane]).await.map(|_| ())
    }

    /// A new window running the agent, in `session` (made when there's none, detached). Nothing takes focus: `-d`
    /// leaves every client on the window it shows. Answers the new pane's ID.
    pub async fn open(&self, session: &str, label: &str, cwd: &str, command: &[String]) -> Result<String, Failure> {
        let target = format!("={session}:");
        let has = self.run(&["has-session", "-t", &format!("={session}")]).await;
        let mut args: Vec<&str> = match has {
            Ok(_) => vec!["new-window", "-d", "-t", &target],
            Err(Failure::NoServer | Failure::NotHere(_)) => {
                vec!["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]
            }
            Err(e) => return Err(e),
        };
        args.extend(["-n", label, "-c", cwd, "-P", "-F", "#{pane_id}", "--"]);
        args.extend(command.iter().map(String::as_str));
        let out = self.run(&args).await?;
        let pane = out.trim().to_string();
        if !pane.starts_with('%') {
            return Err(Failure::Other(format!("tmux named no pane for the new window (it printed {pane:?})")));
        }
        Ok(pane)
    }
}

fn next_buffer() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The command a new window runs: the agent through the person's login shell, so it finds what's on their PATH,
/// with every argument passed as a positional parameter (never spliced into shell text).
pub fn window_command(shell: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut c: Vec<String> = vec![shell.into(), "-lc".into(), "exec \"$0\" \"$@\"".into(), program.into()];
    c.extend(args.iter().cloned());
    c
}

/// The last `n` lines of a capture, after its trailing blank lines (tmux prints every row of the pane).
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    let start = end.saturating_sub(n.max(1));
    let mut s = lines[start..end].join("\n");
    s.push('\n');
    s
}

// ---------------------------------------------------------------- keys

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A key tmux knows by name (`Enter`, `C-c`).
    Named(String),
    /// A character typed as it is.
    Literal(String),
}

/// Fleetlayer's key names (herdr's, lower case) as tmux's. A single character is typed as it is.
pub fn key(name: &str) -> Option<Key> {
    if name.chars().count() == 1 {
        return Some(Key::Literal(name.into()));
    }
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => "Enter",
        "tab" => "Tab",
        "shift+tab" | "btab" | "backtab" => "BTab",
        "backspace" | "bspace" => "BSpace",
        "escape" | "esc" => "Escape",
        "space" => "Space",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PPage",
        "pagedown" | "pgdn" => "NPage",
        "delete" | "del" => "DC",
        "insert" => "IC",
        _ => {
            let rest = ["ctrl+", "ctrl-", "c-"].iter().find_map(|p| lower.strip_prefix(p))?;
            let mut chars = rest.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            return c.is_ascii_alphanumeric().then(|| Key::Named(format!("C-{c}")));
        }
    };
    Some(Key::Named(named.into()))
}

// ---------------------------------------------------------------- panes

/// The fields `list-panes` prints for each pane, tab-separated. Free text (names, the folder) comes last, and a
/// name with a tab in it only loses what follows the tab.
pub const PANE_FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{pane_in_mode}\t#{session_attached}\t#{window_active}\t#{window_index}\t#{pane_index}\t#{automatic-rename}\t#{pane_width}\t#{pane_height}\t#{session_name}\t#{window_name}\t#{pane_current_path}";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pane {
    /// `%12`.
    pub id: String,
    pub pid: u32,
    pub dead: bool,
    /// In copy mode or another mode: keys go to the mode, not the program.
    pub in_mode: bool,
    /// How many clients have its session open.
    pub attached: u32,
    pub window_active: bool,
    pub window_index: u32,
    pub pane_index: u32,
    /// The window's name was given by a person (`automatic-rename` is off), not tmux's guess from the program.
    pub window_named: bool,
    pub width: u32,
    pub height: u32,
    pub session: String,
    pub window: String,
    pub cwd: Option<String>,
}

impl Pane {
    /// A person can see it now: a client has its session open on its window.
    pub fn visible(&self) -> bool {
        self.attached > 0 && self.window_active
    }
}

pub fn parse_pane(line: &str) -> Option<Pane> {
    let f: Vec<&str> = line.splitn(14, '\t').collect();
    if f.len() < 13 || !f[0].starts_with('%') {
        return None;
    }
    let n = |s: &str| s.trim().parse::<u32>().unwrap_or(0);
    Some(Pane {
        id: f[0].into(),
        pid: f[1].trim().parse().ok()?,
        dead: f[2] == "1",
        in_mode: f[3] == "1",
        attached: n(f[4]),
        window_active: f[5] == "1",
        window_index: n(f[6]),
        pane_index: n(f[7]),
        // An empty answer (a tmux that doesn't print options as formats) reads as tmux's default, on.
        window_named: f[8] == "0",
        width: n(f[9]),
        height: n(f[10]),
        session: f[11].into(),
        window: f[12].into(),
        cwd: f.get(13).map(|s| s.to_string()).filter(|s| !s.is_empty()),
    })
}

/// One agent in a tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub pane: Pane,
    /// The agent's process.
    pub pid: u32,
    pub harness: String,
    /// The harness's own session ID, when it's known (Claude Code's from its session registry, Codex's from the
    /// transcript it has open).
    pub session_id: Option<String>,
    pub status: Status,
    /// The name a person gave the Claude Code session (`--name`, `/rename`).
    pub claude_name: Option<String>,
    /// Its name in the fleet (`assign_tabs`).
    pub tab: String,
}

/// Each agent's name in the fleet: the name a person gave the Claude Code session; else the window's name when a
/// person named it; else the tmux session's name, or `<session>:<window>` when the session runs several agents. Two
/// agents in one window add `.<pane>`, as tmux's own targets do.
pub fn assign_tabs(agents: &mut [Agent]) {
    let mut per_session: HashMap<String, usize> = HashMap::new();
    let mut per_window: HashMap<(String, u32), usize> = HashMap::new();
    for a in agents.iter() {
        *per_session.entry(a.pane.session.clone()).or_default() += 1;
        *per_window.entry((a.pane.session.clone(), a.pane.window_index)).or_default() += 1;
    }
    for a in agents.iter_mut() {
        let p = &a.pane;
        let split = per_window[&(p.session.clone(), p.window_index)] > 1;
        a.tab = if let Some(name) = a.claude_name.as_deref().filter(|n| !n.trim().is_empty()) {
            name.to_string()
        } else {
            let base = if p.window_named && !p.window.is_empty() {
                p.window.clone()
            } else if per_session[&p.session] > 1 {
                format!("{}:{}", p.session, p.window_index)
            } else {
                p.session.clone()
            };
            if split { format!("{base}.{}", p.pane_index) } else { base }
        };
    }
}

// ---------------------------------------------------------------- processes

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// The program's name, without its folder.
    pub comm: String,
    /// The first word of its command line, without its folder (what a program that renames itself shows).
    pub argv0: String,
}

fn base(p: &str) -> String {
    Path::new(p).file_name().map_or_else(|| p.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Every process on the machine, from `ps` (Linux and macOS alike).
pub async fn processes() -> Vec<Proc> {
    let run = |fields: &'static [&'static str]| async move {
        let mut c = tokio::process::Command::new("ps");
        c.arg("-A");
        for f in fields {
            c.args(["-o", f]);
        }
        c.stdin(std::process::Stdio::null()).kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(10), c.output()).await {
            Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => String::new(),
        }
    };
    let comms = run(&["pid=", "ppid=", "comm="]).await;
    let args = run(&["pid=", "args="]).await;
    parse_processes(&comms, &args)
}

/// `ps -o pid= -o ppid= -o comm=` and `ps -o pid= -o args=`, joined by process.
pub fn parse_processes(comms: &str, args: &str) -> Vec<Proc> {
    let mut argv0: HashMap<u32, String> = HashMap::new();
    for line in args.lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(first)) = (it.next().and_then(|p| p.parse().ok()), it.next()) {
            argv0.insert(pid, base(first));
        }
    }
    comms
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, comm) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let pid: u32 = pid.parse().ok()?;
            Some(Proc {
                pid,
                ppid: ppid.parse().ok()?,
                comm: base(comm.trim()),
                argv0: argv0.get(&pid).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn normalise(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "claude-code" | "claude code" => "claude".into(),
        _ => n,
    }
}

/// The agent a process is, by its program's name or the name it shows on its command line.
pub fn harness_of(p: &Proc) -> Option<String> {
    [&p.comm, &p.argv0].into_iter().map(|n| normalise(n)).find(|n| HARNESSES.contains(&n.as_str()))
}

/// The agent running in a pane: the nearest process under the pane's own (the pane's process itself included) that
/// is an agent, never looking inside another session manager.
pub fn find_agent(pane_pid: u32, procs: &[Proc]) -> Option<&Proc> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    let mut level: Vec<&Proc> = procs.iter().filter(|p| p.pid == pane_pid).collect();
    for _ in 0..32 {
        if level.is_empty() {
            return None;
        }
        if let Some(found) = level.iter().find(|p| harness_of(p).is_some()) {
            return Some(found);
        }
        level = level
            .iter()
            .filter(|p| {
                let (c, a) = (normalise(&p.comm), normalise(&p.argv0));
                !OTHER_MANAGERS.contains(&c.as_str()) && !OTHER_MANAGERS.contains(&a.as_str())
            })
            .flat_map(|p| children.get(&p.pid).cloned().unwrap_or_default())
            .collect();
    }
    None
}

// ---------------------------------------------------------------- Claude Code's session registry

/// Claude Code registers every interactive session in `~/.claude/sessions/<pid>.json`: its process, its session ID,
/// the name a person gave it and, in recent versions, its state (`busy`, `idle`, `waiting` with what for, `shell`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub pid: u32,
    pub session_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub name_source: Option<String>,
}

pub async fn claude_registrations(dir: &Path) -> Vec<Registration> {
    let mut out = vec![];
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Ok(bytes) = tokio::fs::read(&p).await
            && let Ok(r) = serde_json::from_slice::<Registration>(&bytes)
        {
            out.push(r);
        }
    }
    out
}

/// Claude Code's state as Fleetlayer's: `waiting` is on a person (a permission prompt, a question, a dialog), and
/// `shell` is idle with the input in shell mode. A state added later reads as unknown.
pub fn registry_status(s: &str) -> Status {
    match s {
        "busy" => Status::Working,
        "idle" | "shell" => Status::Idle,
        "waiting" => Status::Blocked,
        _ => Status::Unknown,
    }
}

/// Claude Code's directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"))
}

/// A configured path with a leading `~` (`~` or `~/…`) as a path in the person's home; any other path as it is.
pub fn expand_home(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

/// A Codex session's ID, from the transcript its process has open (`rollout-<time>-<uuid>.jsonl`). Linux only:
/// elsewhere a Codex session stays unnamed (`@<address>`) and is still listed and reachable.
pub fn codex_session(pid: u32) -> Option<String> {
    let rd = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    rd.flatten().find_map(|e| std::fs::read_link(e.path()).ok().and_then(|t| rollout_id(&t.to_string_lossy())))
}

pub fn rollout_id(path: &str) -> Option<String> {
    let name = base(path);
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let shape = id.len() == 36
        && id.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
    shape.then(|| id.to_string())
}

/// An agent's state read off its screen, for agents with no registry state (Codex, an older Claude Code): working
/// while it offers "esc to interrupt", blocked on an approval menu, otherwise idle. Unknown agents are only ever
/// working or unknown, so nothing is typed into a screen this can't read.
pub fn screen_status(harness: &str, screen: &str) -> Status {
    let text = screen.to_ascii_lowercase();
    if text.contains("esc to interrupt") || text.contains("esc to cancel") {
        return Status::Working;
    }
    let menus: &[&str] = match harness {
        "claude" => &["do you want to proceed?", "do you want to make this edit", "do you want to create", "❯ 1."],
        "codex" => &[
            "would you like to run the following command",
            "would you like to make the following edits",
            "yes, proceed",
            "press enter to confirm",
        ],
        _ => return Status::Unknown,
    };
    if menus.iter().any(|m| text.contains(m)) { Status::Blocked } else { Status::Idle }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(pid: u32, ppid: u32, comm: &str, argv0: &str) -> Proc {
        Proc { pid, ppid, comm: comm.into(), argv0: argv0.into() }
    }

    #[test]
    fn addresses_are_pane_ids_without_the_percent() {
        assert_eq!(address("%12"), "p12");
        assert_eq!(pane_id("p12").as_deref(), Some("%12"));
        assert_eq!(pane_id("p"), None);
        assert_eq!(pane_id("w1-3"), None);
        assert_eq!(pane_id("p1;kill-server"), None);
    }

    #[test]
    fn tmux_errors_sort_into_kinds() {
        assert_eq!(classify("no server running on /tmp/tmux-1000/default\n"), Failure::NoServer);
        assert_eq!(classify("error connecting to /tmp/tmux-1000/x (No such file or directory)"), Failure::NoServer);
        assert!(matches!(classify("can't find pane: %99"), Failure::NotHere(_)));
        assert!(matches!(classify("unknown command: frob"), Failure::Other(m) if m == "unknown command: frob"));
        assert_eq!(classify(""), Failure::Other("tmux failed".into()));
    }

    #[test]
    fn a_missing_tmux_says_how_to_point_at_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(spawn_error("/opt/tmux", &gone).starts_with("there's no tmux at /opt/tmux: give the tmux plugin"));
    }

    #[test]
    fn panes_parse_with_free_text_last() {
        let line = "%3\t4242\t0\t1\t2\t1\t2\t0\t0\t200\t50\twork\treview\tthings\t/home/a/code";
        let p = parse_pane(line).unwrap();
        assert_eq!((p.id.as_str(), p.pid, p.dead, p.in_mode, p.attached), ("%3", 4242, false, true, 2));
        assert!(p.window_named && p.visible());
        assert_eq!((p.session.as_str(), p.window.as_str()), ("work", "review"));
        assert_eq!(p.cwd.as_deref(), Some("things\t/home/a/code"), "a tab in a name shifts only what follows it");
        let auto = parse_pane("%4\t1\t0\t0\t0\t0\t1\t1\t1\t80\t24\ts\tclaude\t/w").unwrap();
        assert!(!auto.window_named && !auto.visible());
        assert!(parse_pane("garbage").is_none());
    }

    #[test]
    fn processes_parse_from_ps() {
        let comms = "  1     0 systemd\n 4242     1 bash\n 4300  4242 claude\n 4400 4242 /usr/local/bin/My Tool\n";
        let args = "1 /sbin/init\n4242 -bash\n4300 claude --name review\n4400 /usr/local/bin/My Tool --x\n";
        let ps = parse_processes(comms, args);
        assert_eq!(ps.len(), 4);
        assert_eq!(ps[2], proc_(4300, 4242, "claude", "claude"));
        assert_eq!(ps[3].comm, "My Tool");
    }

    #[test]
    fn the_agent_is_the_nearest_one_under_the_pane() {
        let ps = vec![
            proc_(10, 1, "bash", "-bash"),
            proc_(11, 10, "claude", "claude"),
            proc_(12, 11, "bash", "bash"),
            proc_(13, 12, "claude", "claude"),
            // Codex's npm wrapper runs the real binary as a child.
            proc_(20, 1, "zsh", "zsh"),
            proc_(21, 20, "node", "node"),
            proc_(22, 21, "codex", "/usr/lib/codex/codex"),
            // herdr run inside a tmux pane: its agents are herdr's.
            proc_(30, 1, "bash", "bash"),
            proc_(31, 30, "herdr", "herdr"),
            proc_(32, 31, "claude", "claude"),
            proc_(40, 1, "vim", "vim"),
            // Claude Code on macOS runs as its version-named binary but shows `claude` on its command line.
            proc_(50, 1, "2.1.332", "claude"),
        ];
        assert_eq!(find_agent(10, &ps).map(|p| p.pid), Some(11), "not the claude a tool call started");
        assert_eq!(find_agent(20, &ps).map(|p| p.pid), Some(22));
        assert_eq!(find_agent(30, &ps), None, "herdr's sessions aren't listed twice");
        assert_eq!(find_agent(40, &ps), None);
        assert_eq!(find_agent(50, &ps).and_then(harness_of).as_deref(), Some("claude"));
    }

    fn agent(session: &str, window: u32, pane: u32, window_name: &str, named: bool, claude: Option<&str>) -> Agent {
        Agent {
            pane: Pane {
                id: format!("%{window}{pane}"),
                session: session.into(),
                window_index: window,
                pane_index: pane,
                window: window_name.into(),
                window_named: named,
                ..Default::default()
            },
            pid: 1,
            harness: "claude".into(),
            session_id: None,
            status: Status::Idle,
            claude_name: claude.map(str::to_string),
            tab: String::new(),
        }
    }

    #[test]
    fn tab_names_follow_what_a_person_named() {
        let mut a = vec![
            agent("work", 1, 0, "claude", false, Some("planning")),
            agent("work", 2, 0, "review", true, None),
            agent("work", 3, 0, "claude", false, None),
            agent("solo", 1, 0, "claude", false, None),
            agent("split", 1, 0, "pair", true, None),
            agent("split", 1, 1, "pair", true, None),
        ];
        assign_tabs(&mut a);
        let tabs: Vec<&str> = a.iter().map(|a| a.tab.as_str()).collect();
        assert_eq!(tabs, ["planning", "review", "work:3", "solo", "pair.0", "pair.1"]);
    }

    #[test]
    fn keys_map_to_tmux_names() {
        assert_eq!(key("enter"), Some(Key::Named("Enter".into())));
        assert_eq!(key("backspace"), Some(Key::Named("BSpace".into())));
        assert_eq!(key("shift+tab"), Some(Key::Named("BTab".into())));
        assert_eq!(key("ctrl+c"), Some(Key::Named("C-c".into())));
        assert_eq!(key("2"), Some(Key::Literal("2".into())));
        assert_eq!(key(";"), Some(Key::Literal(";".into())), "typed, never read as tmux's command separator");
        assert_eq!(key("ctrl+;"), None);
        assert_eq!(key("kill-server"), None);
    }

    #[test]
    fn claude_states_read_as_fleetlayer_statuses() {
        assert_eq!(registry_status("busy"), Status::Working);
        assert_eq!(registry_status("idle"), Status::Idle);
        assert_eq!(registry_status("shell"), Status::Idle);
        assert_eq!(registry_status("waiting"), Status::Blocked);
        assert_eq!(registry_status("napping"), Status::Unknown);
    }

    #[test]
    fn screens_read_as_statuses() {
        assert_eq!(screen_status("codex", "• Working (3s • esc to interrupt)"), Status::Working);
        assert_eq!(
            screen_status("codex", "Would you like to run the following command?\n› 1. Yes, proceed"),
            Status::Blocked
        );
        assert_eq!(screen_status("codex", "› Ask Codex to do anything"), Status::Idle);
        assert_eq!(screen_status("claude", " Do you want to proceed?\n ❯ 1. Yes"), Status::Blocked);
        assert_eq!(screen_status("aider", "> "), Status::Unknown, "an agent whose screen isn't known is never idle");
    }

    #[test]
    fn codex_transcripts_name_the_session() {
        let p =
            "/home/a/.codex/sessions/2026/11/28/rollout-2026-11-28T10-00-00-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl";
        assert_eq!(rollout_id(p).as_deref(), Some("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"));
        assert_eq!(rollout_id("/x/rollout-short.jsonl"), None);
        assert_eq!(rollout_id("/x/other.jsonl"), None);
    }

    #[test]
    fn captures_lose_their_trailing_blank_rows() {
        assert_eq!(last_lines("a\nb\nc\n\n\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\n", 10), "a\n");
        assert_eq!(last_lines("", 10), "\n");
    }

    #[test]
    fn new_windows_run_the_agent_through_the_login_shell_without_splicing() {
        let c = window_command("/bin/zsh", "claude", &["--name".into(), "a b; rm -rf ~".into()]);
        assert_eq!(c, ["/bin/zsh", "-lc", "exec \"$0\" \"$@\"", "claude", "--name", "a b; rm -rf ~"]);
    }

    #[test]
    fn a_claude_dir_with_a_tilde_is_in_the_home() {
        let home = Path::new("/home/ada");
        assert_eq!(expand_home(home, "~/.claude".into()), PathBuf::from("/home/ada/.claude"));
        assert_eq!(expand_home(home, "~".into()), PathBuf::from("/home/ada"));
        assert_eq!(expand_home(home, "/srv/claude".into()), PathBuf::from("/srv/claude"));
        assert_eq!(
            expand_home(home, "~bob/.claude".into()),
            PathBuf::from("~bob/.claude"),
            "another user's home isn't guessed"
        );
    }
}
