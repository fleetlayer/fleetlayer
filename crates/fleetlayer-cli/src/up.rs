//! `fleetlayer up`, `down` and `services`: everything on one machine with one command.
//!
//! `up` creates what's missing (never overwriting a file that exists), then runs each piece as a user service
//! (launchd on macOS, systemd --user on Linux), which restarts it when it crashes:
//!
//! - the control plane (`dev.fleetlayer.control`), unless `--join` names one elsewhere: state in
//!   `~/.local/state/fleetlayer/control/`;
//! - the node (`fleetlayer-node install`), with the Claude sign-in hub and pages as its plugins: `node.yaml`;
//! - the gateway (`dev.fleetlayer.gateway`): `<name>.localhost:7680` for every service and the status page:
//!   `gateway.yaml`.
//!
//! Every port binds to 127.0.0.1. `down` stops the three services and leaves every file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(clap::Args, Debug)]
pub struct UpArgs {
    /// Join a control plane elsewhere (`wss://control.example/v1/link`) instead of running one here. Needs
    /// --token-file: the node token from `fleetlayer-control node add <name>` on that machine.
    #[arg(long, requires = "token_file")]
    pub join: Option<String>,
    /// The node's token for --join (a file, mode 0600).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Don't run the Claude sign-in hub here.
    #[arg(long)]
    pub no_hub: bool,
    /// Don't run pages here.
    #[arg(long)]
    pub no_pages: bool,
    /// Also summarise this machine's sessions (it runs Claude Code for each summary, so it spends tokens).
    #[arg(long)]
    pub summaries: bool,
    /// The session (`tab` or `tab@node`) that hears about anything going down.
    #[arg(long)]
    pub alerts_to: Option<String>,
    /// The gateway's port. Default 7680, or 7690 for a second instance (`$FLEETLAYER_INSTANCE`).
    #[arg(long = "port")]
    pub port_arg: Option<u16>,
    /// The control plane's port, when it runs here; its hooks listen on the next one. Default 7600, or 7700 for a
    /// second instance.
    #[arg(long = "control-port")]
    pub control_port_arg: Option<u16>,
    #[arg(skip)]
    pub port: u16,
    #[arg(skip)]
    pub control_port: u16,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("fleetlayer")
}

fn state_dir() -> PathBuf {
    home().join(".local/state/fleetlayer")
}

fn macos() -> bool {
    cfg!(target_os = "macos")
}

/// A binary on PATH, or next to this one.
fn find_bin(name: &str) -> anyhow::Result<PathBuf> {
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
        && dir.join(name).is_file()
    {
        // Prefer the PATH symlink when it is the same file, so upgrades that swap symlinks reach the service.
        let path = std::env::var_os("PATH").unwrap_or_default();
        for d in std::env::split_paths(&path) {
            let c = d.join(name);
            if c.canonicalize().ok() == dir.join(name).canonicalize().ok() && c.is_file() {
                return Ok(c);
            }
        }
        return Ok(dir.join(name));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for d in std::env::split_paths(&path) {
        let c = d.join(name);
        if c.is_file() {
            return Ok(c);
        }
    }
    anyhow::bail!(
        "{name} isn't installed (not on PATH or next to fleetlayer). Install Fleetlayer with install.sh first."
    )
}

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write `text` to `path` (mode `mode`) unless it exists. True when written.
fn create(path: &Path, text: &str, mode: u32) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path) {
        Ok(mut f) => {
            f.write_all(text.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::anyhow!("writing {}: {e}", path.display())),
    }
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    Ok(())
}

fn run(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().map_err(|e| anyhow::anyhow!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("{cmd:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).expect("a string serialises")
}

// ------------------------------------------------------------------ user services

pub struct Unit {
    /// `control` → `dev.fleetlayer.control` / `fleetlayer-control.service`.
    pub name: &'static str,
    pub description: &'static str,
    pub args: Vec<String>,
}

impl Unit {
    /// `dev.fleetlayer.control`, or `dev.fleetlayer.<instance>.control` for a second instance.
    fn label(&self) -> String {
        fleetlayer_protocol::local::launchd_label(self.name, fleetlayer_protocol::local::instance().as_deref())
    }

    /// `fleetlayer-control.service`, or `fleetlayer-<instance>-control.service`.
    fn unit(&self) -> String {
        fleetlayer_protocol::local::systemd_unit(self.name, fleetlayer_protocol::local::instance().as_deref())
    }

    /// What a second instance adds to the environment: its own HOME (the service manager would set the account's)
    /// and its name.
    fn extra_env() -> Vec<(String, String)> {
        match fleetlayer_protocol::local::instance() {
            Some(i) => vec![("HOME".into(), home().display().to_string()), ("FLEETLAYER_INSTANCE".into(), i)],
            None => vec![],
        }
    }

    fn file(&self) -> PathBuf {
        if macos() {
            home().join("Library/LaunchAgents").join(format!("{}.plist", self.label()))
        } else {
            home().join(".config/systemd/user").join(self.unit())
        }
    }

    fn log(&self) -> PathBuf {
        home().join(format!("Library/Logs/fleetlayer-{}.log", self.name))
    }

    pub fn render(&self, path_env: &str) -> String {
        if macos() {
            let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let args: String = self.args.iter().map(|a| format!("    <string>{}</string>\n", esc(a))).collect();
            let env: String = Self::extra_env()
                .iter()
                .map(|(k, v)| format!("    <key>{k}</key>\n    <string>{}</string>\n", esc(v)))
                .collect();
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {desc}, written by `fleetlayer up`. `fleetlayer down` removes it. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>EnvironmentVariables</key>
  <dict>
{env}    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
                desc = self.description,
                label = self.label(),
                path = esc(path_env),
                log = esc(&self.log().display().to_string()),
            )
        } else {
            let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"));
            let exec: Vec<String> = self.args.iter().map(|a| quote(a)).collect();
            let env: String = Self::extra_env()
                .iter()
                .map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}"))))
                .collect();
            format!(
                "# {desc}, written by `fleetlayer up`. `fleetlayer down` removes it.\n[Unit]\nDescription={desc}\n\n[Service]\nExecStart={exec}\n{env}Environment={path}\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
                desc = self.description,
                exec = exec.join(" "),
                path = quote(&format!("PATH={path_env}")),
            )
        }
    }

    /// Write the file and (re)start the service.
    pub fn install(&self) -> anyhow::Result<()> {
        let path_env = service_path_env();
        let file = self.file();
        std::fs::create_dir_all(file.parent().expect("a unit file has a directory"))?;
        std::fs::write(&file, self.render(&path_env))?;
        if macos() {
            let domain = format!("gui/{}", uid());
            let service = format!("{domain}/{}", self.label());
            let _ = Command::new("launchctl").args(["bootout", &service]).output();
            // bootout returns before launchd has unloaded it; bootstrapping too soon fails with "5: Input/output error".
            for _ in 0..20 {
                if !Command::new("launchctl").args(["print", &service]).output().is_ok_and(|o| o.status.success()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file)).is_err() {
                std::thread::sleep(Duration::from_secs(3));
                run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
            }
        } else {
            let unit = self.unit();
            // systemd reads user units only from the account's own folders, so a second instance's file, under its
            // own HOME, is linked in.
            if fleetlayer_protocol::local::instance().is_some() {
                run(Command::new("systemctl").args(["--user", "link", "--force"]).arg(&file))?;
            }
            run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            run(Command::new("systemctl").args(["--user", "enable", &unit]))?;
            run(Command::new("systemctl").args(["--user", "restart", &unit]))?;
        }
        Ok(())
    }

    /// Stop the service and remove its file. True when it was installed.
    pub fn uninstall(&self) -> anyhow::Result<bool> {
        if macos() {
            let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", uid(), self.label())]).output();
        } else {
            let unit = self.unit();
            let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit]).output();
        }
        match std::fs::remove_file(self.file()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn where_logs(&self) -> String {
        if macos() { self.log().display().to_string() } else { format!("journalctl --user -u {}", self.unit()) }
    }
}

fn uid() -> String {
    run(Command::new("id").arg("-u")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "501".into())
}

// ------------------------------------------------------------------ the pieces' files

pub struct Layout {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Layout {
    fn node_yaml(&self) -> PathBuf {
        std::env::var_os("FLEETLAYER_CONFIG").map(PathBuf::from).unwrap_or_else(|| self.config.join("node.yaml"))
    }
    fn gateway_yaml(&self) -> PathBuf {
        self.config.join("gateway.yaml")
    }
    fn node_token(&self) -> PathBuf {
        self.config.join("node-token")
    }
    fn control(&self) -> PathBuf {
        self.state.join("control")
    }
}

/// The node.yaml `up` writes for a machine that has none.
/// The session plugins `up` adds: herdr's when herdr is installed, Claude Code's when Claude Code is, summaries when
/// asked for. herdr and summaries carry the absolute path of the program they run (herdr, claude), so a service PATH
/// that differs from the shell's can't hide it.
#[derive(Debug, Clone, Default)]
pub struct SessionPlugins {
    pub herdr: Option<PathBuf>,
    pub claude_code: bool,
    pub summaries: Option<PathBuf>,
}

pub fn node_yaml_text(
    control_url: &str,
    token_file: &Path,
    gateway: &str,
    hub: Option<&Path>,
    pages: Option<(&Path, &Path, u16)>,
    sessions: &SessionPlugins,
) -> String {
    let mut y = format!(
        "# Written by `fleetlayer up`. Yours to edit: `fleetlayer up` never overwrites it.\ncontrol:\n  url: {}\n  token_file: {}\n# The gateway's own health is part of this node's (`fleetlayer status`).\ngateway: {}\nplugins:\n",
        yaml_str(control_url),
        yaml_str(&token_file.display().to_string()),
        yaml_str(gateway),
    );
    if let Some(login) = hub {
        y.push_str(&format!(
            r#"  # The Claude sign-in hub: sign your Claude accounts in once, at http://claude-hub.localhost (see the status page).
  # It only collects sign-ins; add this machine's node name to targets to use them here.
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    grants: [sync.publish, reading.publish]
    config:
      role: hub
      targets: []
      hub:
        listen: 127.0.0.1:0
        login_file: {}
        trusted_proxies: [127.0.0.1]
"#,
            yaml_str(&login.display().to_string())
        ));
    }
    if let Some((login, space, port)) = pages {
        let base =
            if port == 80 { "http://pages.localhost".to_string() } else { format!("http://pages.localhost:{port}") };
        y.push_str(&format!(
            r#"  # Pages: any folder in a space is a page at http://pages.localhost/<space>/<folder>/.
  - name: pages
    command: fleetlayer-pages
    config:
      listen: 127.0.0.1:0
      base_url: {}
      apps_domain: apps.localhost
      login_file: {}
      trusted_proxies: [127.0.0.1]
      spaces:
        personal: {{ dir: {}, apps: {} }}
"#,
            yaml_str(&base),
            yaml_str(&login.display().to_string()),
            yaml_str(&space.join("pages").display().to_string()),
            yaml_str(&space.join("apps").display().to_string()),
        ));
    }
    if let Some(herdr) = &sessions.herdr {
        y.push_str(&format!(
            "  # Your herdr tabs, so the fleet sees and reaches every session in them.\n  - name: herdr\n    command: fleetlayer-herdr\n    config:\n      program: {}\n",
            yaml_str(&herdr.display().to_string())
        ));
    }
    if sessions.claude_code {
        y.push_str("  # Claude Code's own view of each session (status, transcript), and starting sessions.\n  - name: claude-code\n    command: fleetlayer-claude-code\n");
    }
    let hint = if sessions.claude_code {
        "# Flags for every session `fleetlayer session start` opens here (left out: Claude Code's default mode).\n# harnesses:\n#   claude: { args: [--permission-mode, auto] }\n"
    } else {
        ""
    };
    if let Some(claude) = &sessions.summaries {
        y.push_str(&format!(
            "  # A short summary of each session as it goes (runs Claude Code, so it spends tokens).\n  - name: summaries\n    command: fleetlayer-summaries\n    grants: [sessions.watch, summary.publish]\n    config:\n      claude: {}\n",
            yaml_str(&claude.display().to_string())
        ));
    }
    y.push_str(hint);
    y
}

/// The account's own home: where the shell's `~` pointed when it found its tools, even for a second instance whose
/// `HOME` is elsewhere.
fn account_home() -> PathBuf {
    nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten().map(|u| u.dir).unwrap_or_else(home)
}

/// The PATH the user services get (see `service_path`).
fn service_path_env() -> String {
    fleetlayer_protocol::local::service_path(
        &std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        &account_home(),
    )
}

/// A program as the services will find it: an absolute path on their PATH.
fn on_path(name: &str) -> Option<PathBuf> {
    fleetlayer_protocol::local::find_on(&service_path_env(), name)
}

/// Which session plugins this machine can run, and a line for each one left out.
fn detect_sessions(want_summaries: bool) -> (SessionPlugins, Vec<String>) {
    let mut skipped = vec![];
    let herdr_bin = on_path("herdr");
    let herdr = herdr_bin.clone().filter(|_| find_bin("fleetlayer-herdr").is_ok());
    if herdr.is_none() {
        skipped.push(if herdr_bin.is_some() {
            "herdr: fleetlayer-herdr isn't installed next to fleetlayer".to_string()
        } else {
            "herdr: not installed (https://herdr.dev), so the fleet sees no herdr tabs".to_string()
        });
    }
    let claude_bin = on_path("claude").or_else(|| {
        let local = account_home().join(".claude/local/claude");
        local.is_file().then_some(local)
    });
    let claude = claude_bin.is_some();
    let claude_code = claude && find_bin("fleetlayer-claude-code").is_ok();
    if !claude_code {
        skipped.push(if claude {
            "claude-code: fleetlayer-claude-code isn't installed next to fleetlayer".to_string()
        } else {
            "claude-code: Claude Code isn't installed".to_string()
        });
    }
    let summaries = claude_bin.clone().filter(|_| want_summaries && find_bin("fleetlayer-summaries").is_ok());
    if want_summaries && summaries.is_none() {
        skipped.push("summaries: needs Claude Code and fleetlayer-summaries".into());
    }
    (SessionPlugins { herdr, claude_code, summaries }, skipped)
}

/// `local_control`: the port of a control plane running here, if one does.
pub fn gateway_yaml_text(port: u16, local_control: Option<u16>, alerts_to: Option<&str>) -> String {
    let mut y = format!(
        "# Written by `fleetlayer up`. Yours to edit: `fleetlayer up` never overwrites it.\n# Services are http://<name>.localhost:{port}; the status page is http://fleetlayer.localhost:{port}.\nlisten: 127.0.0.1:{port}\n"
    );
    y.push_str("services:\n");
    if let Some(c) = local_control {
        y.push_str(&format!("  control: {{ url: \"http://127.0.0.1:{c}\", health: /healthz }}\n"));
    } else {
        y.push_str("  {}\n");
    }
    y.push_str("alerts:\n");
    match alerts_to {
        Some(s) => y.push_str(&format!("  session: {}\n", yaml_str(s))),
        None => y.push_str("  # session: planning      # a session that hears about anything going down\n"),
    }
    // The node is checked through its socket; heartbeats are for jobs that run on a schedule, pushed the way they
    // push to Gatus on a server.
    y.push_str(
        "heartbeats: {}\n  # backup: { every: 3600, token_file: ~/.config/fleetlayer/backup-heartbeat-token }\n",
    );
    y
}

/// `control.url` in an existing node.yaml.
fn existing_control_url(node_yaml: &Path) -> Option<String> {
    let text = std::fs::read_to_string(node_yaml).ok()?;
    let v: serde_json::Value = yaml_serde::from_str(&text).ok()?;
    v["control"]["url"].as_str().map(String::from)
}

/// A control plane on this machine: a `ws://` address on loopback.
fn is_local_control(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://") else { return false };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// The machine's name, as the node works it out (it may have a name in node.yaml already).
fn node_name(node_bin: &Path) -> anyhow::Result<String> {
    let out = run(Command::new(node_bin).arg("config"))?;
    out.lines()
        .find_map(|l| l.strip_prefix("name: "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("fleetlayer-node config printed no name"))
}

/// A GET over plain TCP to 127.0.0.1, without pulling an HTTP client into the CLI. The body, if it answered 2xx.
async fn local_get(port: u16, host: &str, path: &str) -> Option<String> {
    let mut s = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(("127.0.0.1", port)))
        .await
        .ok()?
        .ok()?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n").as_bytes()).await.ok()?;
    let mut out = vec![];
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.split(' ').nth(1).filter(|c| c.starts_with('2'))?;
    Some(body.to_string())
}

pub async fn up(mut a: UpArgs) -> anyhow::Result<()> {
    // A second instance moves off the first one's ports unless told otherwise.
    let second = fleetlayer_protocol::local::instance().is_some();
    a.port = a.port_arg.unwrap_or(if second { 7690 } else { 7680 });
    a.control_port = a.control_port_arg.unwrap_or(if second { 7700 } else { 7600 });
    let l = Layout { config: config_dir(), state: state_dir() };
    let node_bin = find_bin("fleetlayer-node")?;
    let gateway_bin = find_bin("fleetlayer-gateway")?;
    // A node.yaml that already links elsewhere means this machine joined a fleet: no control plane here.
    let existing_control = existing_control_url(&Layout { config: config_dir(), state: state_dir() }.node_yaml());
    let joined = a.join.is_some() || existing_control.as_deref().is_some_and(|u| !is_local_control(u));
    let control_bin = if joined { None } else { Some(find_bin("fleetlayer-control")?) };
    if !a.no_hub {
        find_bin("fleetlayer-claude-auth-manager")?;
    }
    if !a.no_pages {
        find_bin("fleetlayer-pages")?;
    }
    private_dir(&l.config)?;
    private_dir(&l.state)?;
    let gateway_url = format!("http://127.0.0.1:{}", a.port);
    let mut notes: Vec<String> = vec![];

    // The node's file first: its name decides the control plane's enrolment.
    let node_yaml = l.node_yaml();
    let fresh_node_yaml = !node_yaml.exists();
    let control_url = a.join.clone().unwrap_or_else(|| format!("ws://127.0.0.1:{}/v1/link", a.control_port));
    let token_file = match &a.token_file {
        Some(t) => {
            let dest = l.node_token();
            if t != &dest {
                let token = std::fs::read_to_string(t).map_err(|e| anyhow::anyhow!("reading {}: {e}", t.display()))?;
                if !create(&dest, token.trim(), 0o600)? && std::fs::read_to_string(&dest)?.trim() != token.trim() {
                    anyhow::bail!(
                        "{} already holds another token; remove it first if you mean to replace it",
                        dest.display()
                    );
                }
            }
            dest
        }
        None => l.node_token(),
    };
    if fresh_node_yaml {
        let hub_login = (!a.no_hub).then(|| l.state.join("claude-hub-login"));
        let pages_login = l.state.join("pages/login");
        let space = home().join("Fleetlayer/personal");
        if let Some(login) = &hub_login {
            create(login, &random_hex(24)?, 0o600)?;
        }
        if !a.no_pages {
            private_dir(&l.state.join("pages"))?;
            create(&pages_login, &random_hex(24)?, 0o600)?;
            for d in ["pages", "apps"] {
                std::fs::create_dir_all(space.join(d))?;
            }
            for d in ["pages", "apps"] {
                create(&space.join(d).join(".fleetlayer-space"), "", 0o644)?;
            }
        }
        let pages = (!a.no_pages).then_some((pages_login.as_path(), space.as_path(), a.port));
        let (sessions, skipped) = detect_sessions(a.summaries);
        create(
            &node_yaml,
            &node_yaml_text(&control_url, &token_file, &gateway_url, hub_login.as_deref(), pages, &sessions),
            0o600,
        )?;
        println!("Wrote {}.", node_yaml.display());
        let added: Vec<&str> = [
            ("herdr", sessions.herdr.is_some()),
            ("claude-code", sessions.claude_code),
            ("summaries", sessions.summaries.is_some()),
        ]
        .into_iter()
        .filter_map(|(n, on)| on.then_some(n))
        .collect();
        if !added.is_empty() {
            println!("Session plugins: {}.", added.join(", "));
        }
        for s in skipped {
            println!("Skipped {s}.");
        }
        if !a.summaries && sessions.claude_code {
            notes.push("Session summaries are off (they spend tokens): `fleetlayer up --summaries` on a fresh setup, or add the summaries plugin to node.yaml (docs/plugins/summaries.md).".into());
        }
    } else {
        let text = std::fs::read_to_string(&node_yaml)?;
        if !text.lines().any(|l| l.starts_with("gateway:")) {
            let mut f = std::fs::OpenOptions::new().append(true).open(&node_yaml)?;
            std::io::Write::write_all(
                &mut f,
                format!(
                    "\n# Added by `fleetlayer up`: the gateway's health is part of this node's.\ngateway: {}\n",
                    yaml_str(&gateway_url)
                )
                .as_bytes(),
            )?;
            println!("Added gateway: to {}.", node_yaml.display());
        }
        if !text.lines().any(|l| l.starts_with("control:")) {
            notes.push(format!(
                "{} has no control: section, so this node runs on its own. To link it, add:\n  control:\n    url: {control_url}\n    token_file: {}",
                node_yaml.display(),
                token_file.display()
            ));
        }
        let (sessions, _) = detect_sessions(a.summaries);
        for (plugin, flag) in [
            ("claude-auth-manager", a.no_hub),
            ("pages", a.no_pages),
            ("herdr", sessions.herdr.is_none()),
            ("claude-code", !sessions.claude_code),
            ("summaries", sessions.summaries.is_none()),
        ] {
            if !flag && !text.contains(&format!("name: {plugin}")) {
                notes.push(format!(
                    "{} existed, so `up` left its plugins alone: {plugin} isn't in it. See docs/guides/laptop.md for the section to add.",
                    node_yaml.display()
                ));
            }
        }
    }

    // The control plane, here.
    let mut units = vec![];
    if let Some(control_bin) = &control_bin {
        let dir = l.control();
        private_dir(&dir)?;
        let db = dir.join("control.db");
        create(&dir.join("admin-token"), &random_hex(32)?, 0o600)?;
        create(&dir.join("audit-key"), &random_hex(32)?, 0o600)?;
        if !token_file.exists() {
            let name = node_name(&node_bin)?;
            let token = run(Command::new(control_bin).arg("--db").arg(&db).args(["node", "add", &name]))
                .map_err(|e| anyhow::anyhow!("enrolling node {name}: {e}"))?;
            create(&token_file, token.trim(), 0o600)?;
            println!("Enrolled this machine as node {name} in the control plane.");
        }
        units.push(Unit {
            name: "control",
            description: "Fleetlayer control plane",
            args: vec![
                control_bin.display().to_string(),
                "--db".into(),
                db.display().to_string(),
                "serve".into(),
                "--listen".into(),
                format!("127.0.0.1:{}", a.control_port),
                "--hooks-listen".into(),
                format!("127.0.0.1:{}", a.control_port.saturating_add(1)),
                "--admin-token-file".into(),
                dir.join("admin-token").display().to_string(),
                "--audit-key-file".into(),
                dir.join("audit-key").display().to_string(),
            ],
        });
    } else if a.join.is_some() && !token_file.exists() {
        anyhow::bail!("--join needs the node's token: --token-file <file>");
    } else if let Some(u) = existing_control.filter(|_| a.join.is_none()) {
        println!("This node already links to the control plane at {u}, so none runs here.");
    }

    // The gateway.
    let gateway_yaml = l.gateway_yaml();
    if create(
        &gateway_yaml,
        &gateway_yaml_text(a.port, control_bin.is_some().then_some(a.control_port), a.alerts_to.as_deref()),
        0o600,
    )? {
        println!("Wrote {}.", gateway_yaml.display());
    } else if a.alerts_to.is_some() {
        notes.push(format!(
            "{} existed, so --alerts-to wasn't written: set alerts.session there.",
            gateway_yaml.display()
        ));
    }
    units.push(Unit {
        name: "gateway",
        description: "Fleetlayer gateway",
        args: vec![
            gateway_bin.display().to_string(),
            "--config".into(),
            gateway_yaml.display().to_string(),
            "serve".into(),
        ],
    });

    for u in &units {
        u.install()?;
        println!("Started {} ({}). Logs: {}", u.description, u.label(), u.where_logs());
    }
    let out = Command::new(&node_bin).arg("install").output()?;
    if !out.status.success() {
        anyhow::bail!("fleetlayer-node install: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    println!("Started the node.");
    let lingering = || {
        let user = std::env::var("USER").unwrap_or_default();
        run(Command::new("loginctl").args(["show-user", &user, "-p", "Linger"])).is_ok_and(|o| o.trim() == "Linger=yes")
    };
    if !macos() && !lingering() {
        notes.push("To keep everything running while you're logged out: loginctl enable-linger \"$USER\"".into());
    }

    // Wait until the gateway answers and the plugins have announced themselves.
    let mut want: Vec<&str> = vec![];
    if control_bin.is_some() {
        want.push("control");
    }
    if !a.no_hub && fresh_node_yaml {
        want.push("claude-hub");
    }
    if !a.no_pages && fresh_node_yaml {
        want.push("pages");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut status = None;
    while std::time::Instant::now() < deadline {
        if let Some(body) = local_get(a.port, "fleetlayer.localhost", "/api/status").await
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
        {
            let names: Vec<String> = v["services"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["name"].as_str().map(String::from))
                .collect();
            let done = want.iter().all(|w| names.iter().any(|n| n == w));
            status = Some(v);
            if done {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let host = |n: &str| {
        if a.port == 80 { format!("http://{n}.localhost") } else { format!("http://{n}.localhost:{}", a.port) }
    };
    println!();
    match &status {
        None => println!(
            "The gateway isn't answering yet on 127.0.0.1:{}. Logs: {}",
            a.port,
            units.last().map(|u| u.where_logs()).unwrap_or_default()
        ),
        Some(v) => {
            println!("Fleetlayer is running. Status: {}", host("fleetlayer"));
            for s in v["services"].as_array().into_iter().flatten() {
                println!("  {:<12} {}", s["name"].as_str().unwrap_or(""), s["address"].as_str().unwrap_or(""));
            }
            let names: Vec<&str> =
                v["services"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).collect();
            for w in &want {
                if !names.contains(w) {
                    println!("  {w:<12} not up yet: `fleetlayer status` shows the node's plugins");
                }
            }
        }
    }
    if fresh_node_yaml {
        if !a.no_hub {
            println!("Hub login key: {}", l.state.join("claude-hub-login").display());
        }
        if !a.no_pages {
            println!(
                "Pages login key: {}  Pages go in ~/Fleetlayer/personal/pages/",
                l.state.join("pages/login").display()
            );
        }
    }
    for n in notes {
        println!("\n{n}");
    }
    Ok(())
}

pub fn down() -> anyhow::Result<()> {
    for u in [
        Unit { name: "gateway", description: "Fleetlayer gateway", args: vec![] },
        Unit { name: "control", description: "Fleetlayer control plane", args: vec![] },
    ] {
        if u.uninstall()? {
            println!("Stopped {}.", u.description);
        }
    }
    match find_bin("fleetlayer-node") {
        Ok(node) => {
            let out = Command::new(node).arg("uninstall").output()?;
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }
        Err(e) => eprintln!("{e}"),
    }
    println!("Every file is still there: `fleetlayer up` starts it all again.");
    Ok(())
}

pub fn services(rest: &[String]) -> anyhow::Result<()> {
    let status = Command::new(find_bin("fleetlayer-gateway")?).arg("services").args(rest).status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_files_parse_and_name_what_they_should() {
        let y = node_yaml_text(
            "ws://127.0.0.1:7600/v1/link",
            Path::new("/h/.config/fleetlayer/node-token"),
            "http://127.0.0.1:7680",
            Some(Path::new("/h/.local/state/fleetlayer/claude-hub-login")),
            Some((Path::new("/h/.local/state/fleetlayer/pages/login"), Path::new("/h/Fleetlayer/personal"), 7680)),
            &SessionPlugins { herdr: Some("/h/.local/bin/herdr".into()), claude_code: true, summaries: None },
        );
        let v: serde_json::Value = yaml_serde_from(&y);
        assert_eq!(v["gateway"], "http://127.0.0.1:7680");
        assert_eq!(v["plugins"][0]["config"]["hub"]["listen"], "127.0.0.1:0");
        assert_eq!(v["plugins"][1]["config"]["base_url"], "http://pages.localhost:7680");
        assert_eq!(v["plugins"][1]["config"]["spaces"]["personal"]["dir"], "/h/Fleetlayer/personal/pages");
        assert_eq!(v["plugins"][2]["name"], "herdr");
        assert_eq!(v["plugins"][2]["config"]["program"], "/h/.local/bin/herdr");
        assert_eq!(v["plugins"][3]["command"], "fleetlayer-claude-code");
        assert!(v["plugins"].get(4).is_none(), "summaries only when asked for");
        assert!(y.contains("# harnesses:") && v.get("harnesses").is_none(), "a commented hint only");
        let y = node_yaml_text(
            "ws://x/v1/link",
            Path::new("/t"),
            "http://g",
            None,
            None,
            &SessionPlugins { summaries: Some("/h/.local/bin/claude".into()), ..Default::default() },
        );
        let v: serde_json::Value = yaml_serde_from(&y);
        assert_eq!(v["plugins"][0]["name"], "summaries");
        assert_eq!(v["plugins"][0]["grants"][1], "summary.publish");
        assert_eq!(v["plugins"][0]["config"]["claude"], "/h/.local/bin/claude");
        let g: serde_json::Value = yaml_serde_from(&gateway_yaml_text(7680, Some(7600), Some("planning")));
        assert_eq!(g["listen"], "127.0.0.1:7680");
        assert_eq!(g["services"]["control"]["url"], "http://127.0.0.1:7600");
        assert_eq!(g["alerts"]["session"], "planning");
        let g: serde_json::Value = yaml_serde_from(&gateway_yaml_text(7680, None, None));
        assert!(g["services"].as_object().unwrap().is_empty());
    }

    fn yaml_serde_from(s: &str) -> serde_json::Value {
        yaml_serde::from_str(s).unwrap_or_else(|e| panic!("{e}\n{s}"))
    }

    #[test]
    fn an_existing_control_plane_is_kept() {
        let d = std::env::temp_dir().join(format!("fl-up-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("node.yaml");
        std::fs::write(&f, "name: laptop\ncontrol:\n  url: wss://control.example/v1/link\n  token_file: /x\n").unwrap();
        assert_eq!(existing_control_url(&f).as_deref(), Some("wss://control.example/v1/link"));
        assert!(!is_local_control("wss://control.example/v1/link"));
        assert!(is_local_control("ws://127.0.0.1:7600/v1/link"));
        assert!(is_local_control("ws://localhost:7700/v1/link"));
        assert!(!is_local_control("ws://10.0.0.5:7600/v1/link"));
        std::fs::write(&f, "name: laptop\n").unwrap();
        assert_eq!(existing_control_url(&f), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn units_carry_their_arguments() {
        let u = Unit {
            name: "gateway",
            description: "Fleetlayer gateway",
            args: vec!["/x/fleetlayer-gateway".into(), "serve".into()],
        };
        let text = u.render("/usr/bin:/bin");
        assert!(text.contains("fleetlayer-gateway"));
        assert!(text.contains("/usr/bin:/bin"));
        if !macos() {
            assert!(text.contains("ExecStart=\"/x/fleetlayer-gateway\" \"serve\""), "{text}");
        }
    }
}
