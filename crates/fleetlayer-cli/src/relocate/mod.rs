//! `fleetlayer move <component> <destination>`: move one piece of Fleetlayer to another machine with its identity,
//! keys and data. The order never leaves two copies running or none healthy: show the plan; stop it here; copy its
//! files, sealed to a key the destination made for this move and checked file by file by SHA-256; write its settings
//! there, rewritten for that machine's home folder and address; start it there and check it's healthy; point
//! everything here at its new address (and, for the control plane, every linked node); only then remove the copy
//! here. Anything failing before the health check undoes the destination's side and starts it here again.
//!
//! The destination's half is `fleetlayer move-receive`, run over SSH (`receive.rs`).

mod archive;
mod component;
mod receive;
mod services;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer};
use fleetlayer_protocol::rpc::RpcError;
use fleetlayer_protocol::yaml_text;
use serde_json::{Value, json};

use archive::Manifest;
use component::{Here, Inspected, Kind};
use receive::{HealthRequest, ProbeAnswer, ProbeRequest};
use serde::{Deserialize, Serialize};

pub struct NoRequests;

#[async_trait]
impl Handler for NoRequests {
    async fn request(&self, _peer: &Peer, method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

/// The file the control plane reads as it stops: where it moved to.
pub fn settings_moved_to() -> &'static str {
    "moved-to"
}

#[derive(clap::Args, Debug)]
pub struct MoveArgs {
    /// What to move: control, node, gateway, hub (the Claude accounts hub), pages (with its apps), sign-in, or any
    /// plugin's name in node.yaml.
    pub component: String,
    /// Where to: an SSH destination (`server`, `me@10.0.0.5`, a Host in ~/.ssh/config) with Fleetlayer installed,
    /// or `local:<folder>`: another home folder on this machine (a second instance, `$FLEETLAYER_INSTANCE`).
    pub destination: String,
    /// Print the plan and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Its port there. Default: the port it has here, or its usual one (sign-in 7650, hub 7660, pages 7670).
    #[arg(long)]
    pub port: Option<u16>,
    /// The address it listens on there. Default: the address SSH reached that machine on (its LAN or tailnet
    /// address, never every interface), with --port.
    #[arg(long)]
    pub listen: Option<String>,
    /// The address others reach it at, when that isn't --listen (it's behind a proxy with TLS):
    /// `wss://control.example/v1/link`, `https://pages.example`.
    #[arg(long)]
    pub url: Option<String>,
    /// Keep the old copy here, renamed `<name>.moved-<date>`, instead of deleting it.
    #[arg(long)]
    pub keep_old: bool,
    /// How long it gets to be healthy there, in seconds.
    #[arg(long, default_value_t = 90)]
    pub health_timeout: u64,
    /// Serve plain HTTP on a public address there (no TLS). Without it, a machine reached on a public address is
    /// refused: put a proxy with TLS in front (`--listen 127.0.0.1:<port> --url https://…`) or use its LAN or tailnet
    /// address.
    #[arg(long)]
    pub public: bool,
    /// Finish a move that stopped part-way (its journal in ~/.local/state/fleetlayer/moves/ says how far it got).
    #[arg(long, conflicts_with = "rollback")]
    pub resume: bool,
    /// Undo a move that stopped part-way: remove it there and start it here again.
    #[arg(long)]
    pub rollback: bool,
    /// With --rollback: the other machine can't be reached and you've checked it isn't running there.
    #[arg(long, requires = "rollback")]
    pub force: bool,
}

// ------------------------------------------------------------------ the destination

pub enum Dest {
    Ssh(String),
    Local(PathBuf),
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A second instance's name for a local folder: `move-<folder name>`.
fn instance_for(dir: &Path) -> String {
    let base: String = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dest")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("move-{base}").chars().take(32).collect()
}

impl Dest {
    pub fn parse(s: &str) -> anyhow::Result<Dest> {
        match s.strip_prefix("local:") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                anyhow::ensure!(dir.is_absolute(), "local:<folder> needs an absolute folder");
                anyhow::ensure!(
                    Some(dir.as_path()) != std::env::var_os("HOME").as_deref().map(Path::new),
                    "that's this home folder"
                );
                std::fs::create_dir_all(&dir)?;
                Ok(Dest::Local(dir))
            }
            None => {
                anyhow::ensure!(!s.is_empty() && !s.starts_with('-'), "{s:?} isn't an SSH destination");
                Ok(Dest::Ssh(s.to_string()))
            }
        }
    }

    pub fn name(&self) -> String {
        match self {
            Dest::Ssh(h) => h.clone(),
            Dest::Local(d) => format!("local:{}", d.display()),
        }
    }

    fn is_ssh(&self) -> bool {
        matches!(self, Dest::Ssh(_))
    }

    fn command(&self, args: &[&str]) -> anyhow::Result<Command> {
        Ok(match self {
            Dest::Ssh(host) => {
                let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
                // Process mode goes along: it says how the pieces run in this setup (SSH passes no environment).
                let mode = if std::env::var("FLEETLAYER_MOVE_SERVICES").is_ok_and(|v| v == "process") {
                    "FLEETLAYER_MOVE_SERVICES=process "
                } else {
                    ""
                };
                let mut c = Command::new("ssh");
                c.args(["-o", "BatchMode=yes", host]).arg(format!(
                    "PATH=\"$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin:$PATH\" {mode}exec fleetlayer move-receive {}",
                    quoted.join(" ")
                ));
                c
            }
            Dest::Local(dir) => {
                let mut c = Command::new(std::env::current_exe()?);
                c.arg("move-receive")
                    .args(args)
                    .env("HOME", dir)
                    .env("FLEETLAYER_INSTANCE", instance_for(dir))
                    .env_remove("XDG_CONFIG_HOME")
                    .env_remove("FLEETLAYER_CONFIG")
                    // It is this machine, whatever SSH session this runs in.
                    .env_remove("SSH_CONNECTION");
                c
            }
        })
    }

    /// Run a step there, writing its input with `input`; its JSON answer.
    fn call_with(
        &self,
        args: &[&str],
        input: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
    ) -> anyhow::Result<Value> {
        let mut child = self
            .command(args)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting the step there: {e}"))?;
        let mut stdin = std::io::BufWriter::new(child.stdin.take().expect("piped"));
        let wrote = input(&mut stdin).and_then(|()| Ok(stdin.flush()?));
        drop(stdin);
        let out = child.wait_with_output()?;
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            let err = err.lines().rfind(|l| !l.is_empty()).unwrap_or("").trim_start_matches("fleetlayer: ");
            if err.contains("command not found") || err.contains("move-receive") && err.contains("unrecognized") {
                anyhow::bail!(
                    "{}: Fleetlayer {} or later isn't installed there (install it with install.sh first)",
                    self.name(),
                    env!("CARGO_PKG_VERSION")
                );
            }
            anyhow::bail!("{} ({}): {err}", self.name(), args.first().unwrap_or(&""));
        }
        wrote?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().rev().find(|l| l.starts_with('{')).unwrap_or("{}");
        Ok(serde_json::from_str(line)?)
    }

    fn call(&self, args: &[&str], input: &Value) -> anyhow::Result<Value> {
        self.call_with(args, |w| Ok(w.write_all(serde_json::to_vec(input)?.as_slice())?))
    }
}

// ------------------------------------------------------------------ the move

fn bracket(ip: &str) -> String {
    if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() }
}

/// The control plane's web address from its link URL: `wss://c.example/v1/link` → `https://c.example`.
fn http_from_link(url: &str) -> String {
    let u = url.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    u.strip_suffix("/v1/link").unwrap_or(&u).to_string()
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} kB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

/// The node here: its name and public key, from the running node, else from its files.
async fn node_identity(here: &Here) -> (Option<String>, Option<String>) {
    if let Ok(s) = receive::node_call("node/status").await {
        return (s["node"].as_str().map(String::from), s["publicKey"].as_str().map(String::from));
    }
    let name = crate::up::find_bin("fleetlayer-node").ok().and_then(|bin| {
        let out = Command::new(bin).arg("config").output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("name: ").map(|s| s.trim().to_string()))
    });
    let key = std::fs::read_to_string(here.layout.config.join("node-key"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.split("Public: ").nth(1).map(|s| s.trim().to_string())));
    (name, key)
}

/// A service's route in gateway.yaml's `services:`, replacing its entry (a one-line or a block one).
fn set_route(text: &str, name: &str, url: &str, health: Option<&str>, hosts: &[String]) -> String {
    let mut fields = vec![format!("url: {}", yaml_text::quote(url))];
    if let Some(h) = health {
        fields.push(format!("health: {}", yaml_text::quote(h)));
    }
    if !hosts.is_empty() {
        let hs: Vec<String> = hosts.iter().map(|h| yaml_text::quote(h)).collect();
        fields.push(format!("hosts: [{}]", hs.join(", ")));
    }
    let entry = format!("  {name}: {{ {} }}", fields.join(", "));
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    match yaml_text::section(text, "services") {
        None => {
            lines.push("services:".into());
            lines.push(entry);
        }
        Some((s, e)) => {
            // `services: {}` or a lone `{}` line: an empty map, replaced by the block.
            if lines[s].trim_end() != "services:" {
                lines[s] = "services:".into();
            }
            let mut e = e;
            if let Some(i) = (s + 1..e).find(|&i| lines[i].trim() == "{}") {
                lines.remove(i);
                e -= 1;
            }
            let start = (s + 1..e).find(|&i| {
                let l = &lines[i];
                l.len() - l.trim_start().len() == 2 && l.trim_start().split_once(':').is_some_and(|(k, _)| k == name)
            });
            match start {
                Some(i) => {
                    let mut j = i + 1;
                    while j < e && (lines[j].trim().is_empty() || lines[j].len() - lines[j].trim_start().len() > 2) {
                        j += 1;
                    }
                    lines.splice(i..j, [entry]);
                }
                None => lines.insert(e, entry),
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn write_keeping_mode(path: &Path, text: &str) -> anyhow::Result<()> {
    Ok(fleetlayer_protocol::local::write_atomic(path, text, 0o600)?)
}

/// How far a move got, in its journal (`~/.local/state/fleetlayer/moves/<component>.json` on the machine it leaves),
/// so one that stopped part-way can be finished (`--resume`) or undone (`--rollback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// Stopped here; nothing confirmed there.
    Stopped,
    /// Its files are there, checked.
    Copied,
    /// Healthy there and confirmed: nodes may already have saved the new address, so it isn't undone any more.
    Healthy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Route {
    name: String,
    health: Option<String>,
    hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    component: String,
    destination: String,
    session: String,
    stage: Stage,
    listen: Option<String>,
    link_url: Option<String>,
    web_url: Option<String>,
    route: Option<Route>,
    /// Its own state and settings file: removed here once it's moved.
    retire: Vec<PathBuf>,
    /// Content folders copied there and left here (never deleted).
    content_left: Vec<PathBuf>,
    /// node.yaml before its plugin block was commented out.
    node_yaml_before: Option<String>,
    /// Files written here for the move (the control plane's moved-to and proof), removed if it's undone.
    move_files: Vec<PathBuf>,
    health: HealthRequest,
    keep_old: bool,
}

fn journal_path(kind: &Kind) -> PathBuf {
    crate::up::state_dir().join("moves").join(format!("{}.json", kind.id().replace(':', "-")))
}

impl Journal {
    fn load(kind: &Kind) -> Option<Journal> {
        serde_json::from_slice(&std::fs::read(journal_path(kind)).ok()?).ok()
    }

    fn save(&self) -> anyhow::Result<()> {
        let kind = Kind::from_id(&self.component)?;
        let path = journal_path(&kind);
        crate::up::private_dir(path.parent().expect("a journal has a folder"))?;
        fleetlayer_protocol::local::write_atomic(&path, &serde_json::to_string_pretty(self)?, 0o600)?;
        Ok(())
    }

    fn at(&mut self, stage: Stage) -> anyhow::Result<()> {
        self.stage = stage;
        self.save()
    }

    fn remove(&self) {
        if let Ok(kind) = Kind::from_id(&self.component) {
            let _ = std::fs::remove_file(journal_path(&kind));
        }
    }
}

/// For the tests: stop the command right after a stage, as if it was interrupted there.
fn test_exit_after(stage: &str) {
    if std::env::var("FLEETLAYER_MOVE_TEST_EXIT_AFTER").is_ok_and(|v| v == stage) {
        eprintln!("(test) stopping after {stage}");
        std::process::exit(3);
    }
}

/// Stop it here. For the control plane, first leave the move's proof in its folder (it travels with the database)
/// and the new address beside it (it doesn't), for the old copy to tell its nodes as it stops.
fn stop_here(kind: &Kind, here: &Here, i: &Inspected, link_url: Option<&str>, j: &mut Journal) -> anyhow::Result<()> {
    match kind {
        Kind::Control => {
            let dir = i.state.first().expect("the control plane's folder");
            let nonce = crate::up::random_hex(24)?;
            let proof = dir.join("move-proof");
            fleetlayer_protocol::local::write_atomic(
                &proof,
                &json!({"nonce": nonce, "confirmed": false}).to_string(),
                0o600,
            )?;
            let moved_to = dir.join(settings_moved_to());
            fleetlayer_protocol::local::write_atomic(
                &moved_to,
                &format!("{}\n{nonce}\n", link_url.expect("a link URL")),
                0o600,
            )?;
            j.move_files = vec![proof, moved_to];
            j.save()?;
            services::stop(kind)?;
        }
        Kind::Gateway | Kind::Node => services::stop(kind)?,
        Kind::Plugin(name) => {
            let path = here.layout.node_yaml();
            let text = std::fs::read_to_string(&path)?;
            let date = jiff::Timestamp::now().strftime("%Y-%m-%d");
            let out = yaml_text::comment_out_plugin(
                &text,
                name,
                &format!(
                    "Moved to {} on {date} by `fleetlayer move` (left here to read; nothing runs it).",
                    j.destination
                ),
            )
            .ok_or_else(|| anyhow::anyhow!("couldn't find the {name} plugin in {}", path.display()))?;
            j.node_yaml_before = Some(text);
            j.save()?;
            write_keeping_mode(&path, &out)?;
            if services::running(kind) {
                services::start(kind)?;
            }
        }
    }
    Ok(())
}

/// Put it back here as it was: only once the other machine is known not to run it.
fn restart_here(kind: &Kind, here: &Here, j: &Journal) -> anyhow::Result<()> {
    for f in &j.move_files {
        let _ = std::fs::remove_file(f);
    }
    if let Some(text) = &j.node_yaml_before {
        write_keeping_mode(&here.layout.node_yaml(), text)?;
    }
    services::start(kind)
}

fn retire_path(p: &Path, keep: Option<&str>) -> anyhow::Result<String> {
    match keep {
        Some(stamp) => {
            let to = PathBuf::from(format!("{}.moved-{stamp}", p.display()));
            std::fs::rename(p, &to)?;
            Ok(format!("kept as {}", to.display()))
        }
        None if p.is_dir() => {
            std::fs::remove_dir_all(p)?;
            Ok("deleted".into())
        }
        None => {
            std::fs::remove_file(p)?;
            Ok("deleted".into())
        }
    }
}

/// Whether an address is on the open internet: not loopback, a private range (RFC 1918, unique local), link-local or
/// the shared range tailnets use (100.64.0.0/10).
fn is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        std::net::IpAddr::V6(v) => {
            let seg = v.segments()[0];
            !(v.is_loopback() || v.is_unspecified() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80)
        }
    }
}

/// The IP a listen address names, when it names one.
fn listen_ip(listen: &str) -> Option<std::net::IpAddr> {
    let host = listen.rsplit_once(':').map_or(listen, |(h, _)| h);
    host.trim_start_matches('[').trim_end_matches(']').parse().ok()
}

/// Undo it there, and only then start it here again: never two copies running.
fn fail_back(kind: &Kind, here: &Here, dest: &Dest, j: &Journal, force: bool) -> anyhow::Result<()> {
    match dest.call(&["undo", &j.session], &json!({})) {
        Ok(_) => {}
        Err(e) if force => eprintln!("Couldn't undo it there ({e}); starting it here anyway (--force)."),
        Err(e) => anyhow::bail!(
            "couldn't confirm it's stopped on {} ({e}), so it isn't started here: two copies must never run. Once you've checked it isn't running there: `fleetlayer move {} {} --rollback --force`",
            dest.name(),
            component_arg(kind),
            dest.name()
        ),
    }
    restart_here(kind, here, j)?;
    j.remove();
    Ok(())
}

fn component_arg(kind: &Kind) -> String {
    match kind {
        Kind::Plugin(n) => n.clone(),
        k => k.id(),
    }
}

/// Start, check, confirm, repoint and retire: the rest of a move once its files are there.
async fn finish(kind: &Kind, here: &Here, dest: &Dest, mut j: Journal) -> anyhow::Result<()> {
    if j.stage < Stage::Healthy {
        let attempt = async {
            dest.call(&["start", &j.session], &json!({}))?;
            println!("Started it there; checking its health…");
            dest.call(&["health", &j.session], &serde_json::to_value(&j.health)?)?;
            dest.call(&["confirm", &j.session], &json!({}))?;
            anyhow::Ok(())
        };
        if let Err(e) = attempt.await {
            eprintln!("The move failed ({e}); undoing it there and starting it here again.");
            fail_back(kind, here, dest, &j, false)?;
            anyhow::bail!("not moved: {e}. It's running here again, as before.");
        }
        j.at(Stage::Healthy)?;
        println!("It's healthy there.");
    }
    test_exit_after("healthy");

    // Repoint.
    let mut restart_gateway = false;
    match kind {
        Kind::Control => {
            let new = j.link_url.clone().expect("a link URL");
            let node_yaml = here.layout.node_yaml();
            if let Some(t) = here.node_yaml_text() {
                let v: Value = yaml_serde::from_str(&t).unwrap_or_default();
                let old = v["control"]["url"].as_str().unwrap_or("");
                if old != new
                    && (old.contains("://127.0.0.1") || old.contains("://localhost") || old.contains("://[::1]"))
                    && let Some(out) = yaml_text::set_in_section(&t, "control", "url", &new)
                {
                    write_keeping_mode(&node_yaml, &out)?;
                    println!("This node now links to {new} ({}).", node_yaml.display());
                }
            }
            let gw = here.layout.gateway_yaml();
            if let Ok(t) = std::fs::read_to_string(&gw) {
                let web = j.web_url.as_deref().unwrap_or("");
                write_keeping_mode(&gw, &set_route(&t, "control", web, Some("/healthz"), &[]))?;
                restart_gateway = true;
            }
            // This node follows by itself once the move is confirmed; if it isn't linked soon, a restart reads
            // node.yaml.
            if services::running(&Kind::Node) {
                let mut linked = false;
                for _ in 0..30 {
                    if receive::node_call("node/status").await.is_ok_and(|s| s["control"]["connected"] == true) {
                        linked = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if !linked {
                    services::start(&Kind::Node)?;
                }
            }
        }
        Kind::Plugin(_) => {
            if let (Some(r), Some(u)) = (&j.route, &j.web_url) {
                let gw = here.layout.gateway_yaml();
                if let Ok(t) = std::fs::read_to_string(&gw) {
                    write_keeping_mode(&gw, &set_route(&t, &r.name, u, r.health.as_deref(), &r.hosts))?;
                    restart_gateway = true;
                    println!("This machine's gateway now sends {} to {u}.", r.name);
                }
            }
        }
        Kind::Gateway => {
            if let Some(t) = here.node_yaml_text() {
                let u = j.web_url.clone().unwrap_or_default();
                write_keeping_mode(&here.layout.node_yaml(), &yaml_text::set_top(&t, "gateway", &u))?;
                if services::running(&Kind::Node) {
                    services::start(&Kind::Node)?;
                }
            }
        }
        Kind::Node => {}
    }
    if restart_gateway && services::running(&Kind::Gateway) {
        services::start(&Kind::Gateway)?;
    }

    // Retire its own state here, nothing else.
    let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M").to_string();
    let keep = j.keep_old.then_some(stamp.as_str());
    for p in j.retire.iter().filter(|p| std::fs::symlink_metadata(p).is_ok()) {
        match retire_path(p, keep) {
            Ok(what) => println!("  {}: {what}", p.display()),
            Err(e) => eprintln!("  {}: couldn't remove it ({e}); remove it by hand", p.display()),
        }
    }
    for p in &j.content_left {
        println!("  {}: copied there and left here (yours to keep or delete)", p.display());
    }
    let _ = dest.call(&["done", &j.session], &json!({}));
    j.remove();
    println!(
        "Moved {} to {}{}.",
        kind.describe(),
        dest.name(),
        match (kind, &j.link_url, &j.web_url) {
            (Kind::Control, Some(l), _) => format!(": nodes link to {l}"),
            (_, _, Some(u)) => format!(", at {u}"),
            _ => String::new(),
        }
    );
    Ok(())
}

pub async fn run(a: MoveArgs) -> anyhow::Result<()> {
    let here = Here::new();
    let kind = Kind::parse(&a.component);
    if a.resume || a.rollback {
        let j = Journal::load(&kind).ok_or_else(|| {
            anyhow::anyhow!("no unfinished move of {} here ({})", kind.describe(), journal_path(&kind).display())
        })?;
        let dest = Dest::parse(&j.destination)?;
        anyhow::ensure!(
            dest.name() == Dest::parse(&a.destination)?.name(),
            "the unfinished move of {} was to {}, not {}",
            kind.describe(),
            j.destination,
            a.destination
        );
        if a.rollback {
            anyhow::ensure!(
                j.stage < Stage::Healthy,
                "it's running on {} and the move was confirmed, so nodes may already use its new address: finish it with --resume, or move it back with `fleetlayer move`",
                j.destination
            );
            fail_back(&kind, &here, &dest, &j, a.force)?;
            println!(
                "Rolled back: {} runs here again, and nothing of it is left on {}.",
                kind.describe(),
                j.destination
            );
            return Ok(());
        }
        anyhow::ensure!(
            j.stage >= Stage::Copied,
            "its copy to {} hadn't finished: undo it with --rollback, then move it again",
            j.destination
        );
        return finish(&kind, &here, &dest, j).await;
    }
    if let Some(j) = Journal::load(&kind) {
        anyhow::bail!(
            "an earlier move of {} to {} didn't finish (it got as far as {:?}): `fleetlayer move {} {} --resume` finishes it, `--rollback` undoes it",
            kind.describe(),
            j.destination,
            j.stage,
            component_arg(&kind),
            j.destination
        );
    }
    let announced: Vec<Value> = match receive::node_call("node/services").await {
        Ok(v) => v["services"].as_array().cloned().unwrap_or_default(),
        Err(_) => vec![],
    };
    let mut i = component::inspect(&here, &kind, &announced)?;
    let dest = Dest::parse(&a.destination)?;
    let (node_name, public_key) = node_identity(&here).await;

    let mut notes = std::mem::take(&mut i.notes);
    let state_files = component::files_under(&i.state, &mut notes)?;
    let mut content_files_by_dir = vec![];
    for d in &i.content {
        content_files_by_dir.push((d.clone(), component::files_under(std::slice::from_ref(d), &mut notes)?));
    }
    let rel = |p: &Path| component::rel(&here.home, p);
    let settings_rel: Vec<String> = i.settings_file.iter().map(|(f, _)| rel(f)).collect::<anyhow::Result<_>>()?;
    let probe_req = ProbeRequest {
        component: kind.id(),
        files: state_files.iter().map(|f| rel(f)).collect::<anyhow::Result<_>>()?,
        content: i.content.iter().map(|d| rel(d)).collect::<anyhow::Result<_>>()?,
        settings: settings_rel.clone(),
        dry_run: a.dry_run,
    };
    let probe: ProbeAnswer = serde_json::from_value(dest.call(&["probe"], &serde_json::to_value(&probe_req)?)?)?;
    let session = probe.session.clone();
    let abort = |why: anyhow::Error| -> anyhow::Error {
        if let Some(id) = &session {
            let _ = dest.call(&["done", id], &json!({}));
        }
        why
    };

    // What can't work, before anything changes.
    let mut problems = vec![];
    if !probe.missing.is_empty() {
        problems
            .push(format!("{} isn't installed there (install Fleetlayer with install.sh)", probe.missing.join(", ")));
    }
    let entries = component::entries_now(&here.home, &state_files);
    for e in &entries {
        if let Some(sha) = probe.existing.get(&e.path)
            && *sha != e.sha256
        {
            problems.push(format!("~/{} is already there with other content", e.path));
        }
    }
    for s in &probe.settings_present {
        problems.push(format!("~/{s} is already there (move it away first)"));
    }
    match &kind {
        Kind::Plugin(name) => match &probe.node_yaml {
            None => problems.push(format!(
                "there's no node there to run {name}: run `fleetlayer up --join …` there first (docs/guides/moving.md)"
            )),
            Some(t) if yaml_text::plugin_block(t, name).is_some() => {
                problems.push(format!("the node there already runs a plugin called {name}"))
            }
            Some(_) => {}
        },
        Kind::Node if probe.node_yaml.is_some() => problems.push(format!(
            "there's a node there already ({}); a Unix user runs one node, so move this one to a user that has none",
            probe.node_name.as_deref().unwrap_or("?")
        )),
        _ => {}
    }
    let mut copied_content = vec![];
    let mut content_files = vec![];
    for (d, files) in &content_files_by_dir {
        let r = rel(d)?;
        if probe.content_present.contains(&r) {
            notes.push(format!("~/{r} is there already (shared or synced), so it isn't copied."));
        } else {
            copied_content.push(d.clone());
            content_files.extend(files.iter().cloned());
        }
    }

    // Its address there.
    let listens = matches!(kind, Kind::Control | Kind::Gateway) || i.listens;
    let listen = if listens {
        let port = a.port.or(i.port).or(match &kind {
            Kind::Plugin(n) => component::default_port(n),
            _ => None,
        });
        match (&a.listen, port) {
            (Some(l), _) => Some(l.clone()),
            (None, Some(p)) => Some(format!("{}:{p}", bracket(&probe.bind_ip))),
            (None, None) => {
                problems.push("it has no fixed port: pass --port".into());
                None
            }
        }
    } else {
        None
    };
    if let Some(l) = &listen
        && let Some(ip) = listen_ip(l)
        && is_public(ip)
        && !a.public
    {
        problems.push(format!(
            "it would serve plain HTTP on {l}, a public address. Put a proxy with TLS in front on that machine and pass `--listen 127.0.0.1:<port> --url https://…` (wss:// for the control plane), reach it by its LAN or tailnet address instead, or pass --public if you really mean it"
        ));
    }
    if dest.is_ssh()
        && let Some(l) = &listen
        && listen_ip(l).is_some_and(|ip| ip.is_loopback())
        && a.url.is_none()
    {
        notes.push(format!(
            "It will listen on {l} there, which only that machine reaches: put a proxy in front and pass --url."
        ));
    }
    let link_url = (kind == Kind::Control)
        .then(|| a.url.clone().unwrap_or_else(|| format!("ws://{}/v1/link", listen.as_deref().unwrap_or(""))));
    let web_url = match (&kind, &a.url, &listen) {
        (Kind::Control, _, _) => link_url.as_deref().map(http_from_link),
        (_, Some(u), _) => Some(u.clone()),
        (_, None, Some(l)) => Some(format!("http://{l}")),
        _ => None,
    };
    let dest_home = PathBuf::from(&probe.home);
    let mut all_files = state_files.clone();
    all_files.extend(content_files.iter().cloned());
    let settings = component::settings_for(&kind, &i, &here, &dest_home, listen.as_deref(), node_name.as_deref())?;

    // The plan.
    let total: u64 = all_files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    let mut plan = vec![];
    plan.push(match &kind {
        Kind::Plugin(_) => {
            "Stop it here: its block in node.yaml is commented out and the node restarts without it.".to_string()
        }
        Kind::Control => {
            "Stop it here, telling each linked node its new address and the move's proof as it goes.".into()
        }
        _ => "Stop it here.".into(),
    });
    plan.push(format!(
        "Copy {} files ({}), sealed to a key {} made for this move, each checked by SHA-256 there:\n{}",
        all_files.len(),
        human_size(total),
        dest.name(),
        all_files
            .iter()
            .take(12)
            .map(|f| format!("       ~/{}", rel(f).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n")
            + &if all_files.len() > 12 {
                format!("\n       … and {} more", all_files.len() - 12)
            } else {
                String::new()
            }
    ));
    for s in &settings {
        plan.push(match s {
            archive::Setting::File { path, .. } => format!(
                "Write ~/{path} there{}.",
                listen.as_deref().map(|l| format!(", listening on {l}")).unwrap_or_default()
            ),
            archive::Setting::Plugin { name, .. } => format!(
                "Add the {name} plugin to the node.yaml there{}.",
                listen.as_deref().map(|l| format!(", listening on {l}")).unwrap_or_default()
            ),
        });
    }
    plan.push("Start it there and wait until it's healthy (else undo all of this and start it here again).".into());
    plan.push(match &kind {
        Kind::Control => format!(
            "Point everything at {}: linked nodes switch once the move is confirmed, and this machine's node.yaml and gateway.yaml are updated.",
            link_url.as_deref().unwrap_or("")
        ),
        Kind::Plugin(_) => match (&i.route, &web_url) {
            (Some(r), Some(u)) => format!("Route {} on this machine's gateway to {u}.", r.name),
            _ => "Nothing to repoint: it serves no web page.".into(),
        },
        Kind::Gateway => format!("Set gateway: in this machine's node.yaml to {}.", web_url.as_deref().unwrap_or("")),
        Kind::Node => format!(
            "Nothing to repoint: the node keeps its name ({}) and key, so the control plane and every node that seals to it carry on.",
            node_name.as_deref().unwrap_or("?")
        ),
    });
    plan.push(format!(
        "{} its own state here once it's healthy there{}.",
        if a.keep_old { "Keep (renamed .moved-<date>)" } else { "Delete" },
        if copied_content.is_empty() { "" } else { " (the content folders it copied stay here)" }
    ));
    println!("Moving {} from this machine to {} (home {}):", kind.describe(), dest.name(), probe.home);
    for (n, p) in plan.iter().enumerate() {
        println!("  {}. {p}", n + 1);
    }
    for n in &notes {
        println!("  Note: {n}");
    }
    if !problems.is_empty() {
        let err = anyhow::anyhow!("can't move it yet, and nothing was changed:\n  - {}", problems.join("\n  - "));
        return Err(abort(err));
    }
    if a.dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(());
    }
    let id = session.clone().expect("a session when not a dry run");
    let recipient = probe.recipient.clone().expect("a key when not a dry run");
    let mut retire: Vec<PathBuf> = i.state.clone();
    if let Some((f, _)) = &i.settings_file {
        retire.push(f.clone());
    }
    let mut j = Journal {
        component: kind.id(),
        destination: dest.name(),
        session: id.clone(),
        stage: Stage::Stopped,
        listen: listen.clone(),
        link_url: link_url.clone(),
        web_url: web_url.clone(),
        route: i.route.as_ref().map(|r| Route {
            name: r.name.clone(),
            health: r.health.clone(),
            hosts: r.hosts.clone(),
        }),
        retire,
        content_left: copied_content.clone(),
        node_yaml_before: None,
        move_files: vec![],
        health: HealthRequest {
            listen: listen.clone(),
            node_name: (kind == Kind::Node).then(|| node_name.clone()).flatten(),
            public_key: (kind == Kind::Node).then(|| public_key.clone()).flatten(),
            linked: kind == Kind::Node
                && here.node_yaml_text().is_some_and(|t| t.lines().any(|l| l.starts_with("control:"))),
            plugin: match &kind {
                Kind::Plugin(n) => Some(n.clone()),
                _ => None,
            },
            timeout_secs: a.health_timeout,
        },
        keep_old: a.keep_old,
    };
    j.save().map_err(abort)?;

    // Stop it here.
    if let Err(e) = stop_here(&kind, &here, &i, link_url.as_deref(), &mut j) {
        let _ = restart_here(&kind, &here, &j);
        j.remove();
        return Err(abort(e));
    }
    println!("Stopped it here.");

    // Copy. Listed again now it's stopped: SQLite removes its -wal and -shm files as it closes.
    let copy = (|| {
        let mut now = component::files_under(&i.state, &mut vec![])?;
        now.extend(component::files_under(&copied_content, &mut vec![])?);
        let manifest =
            Manifest { component: kind.id(), files: component::entries(&here.home, &now)?, settings: settings.clone() };
        dest.call_with(&["unpack", &id], |w| archive::write_sealed(&recipient, &here.home, &manifest, w))
    })();
    match copy {
        Ok(v) => println!(
            "Copied and checked {} files there ({} already there and identical).",
            v["written"].as_u64().unwrap_or(0),
            v["skipped"].as_u64().unwrap_or(0)
        ),
        Err(e) => {
            eprintln!("The copy failed ({e}); undoing it there and starting it here again.");
            fail_back(&kind, &here, &dest, &j, false)?;
            anyhow::bail!("not moved: {e}. It's running here again, as before.");
        }
    }
    j.at(Stage::Copied)?;
    test_exit_after("copied");
    finish(&kind, &here, &dest, j).await
}

/// `fleetlayer move-receive <step> [args]`: the destination's side.
pub async fn receive(step: &str, args: &[String]) -> anyhow::Result<()> {
    let arg = |n: usize| args.get(n).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{step}: missing argument"));
    match step {
        "probe" => receive::probe(),
        "unpack" => receive::unpack(arg(0)?),
        "start" => receive::start(arg(0)?),
        "health" => receive::health(arg(0)?).await,
        "undo" => receive::undo(arg(0)?).await,
        "confirm" => receive::confirm(arg(0)?),
        "done" => receive::done(arg(0)?),
        "service" => receive::service(arg(0)?, arg(1)?),
        _ => anyhow::bail!("unknown step {step}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_written_into_gateway_yaml() {
        let up = "# Written by `fleetlayer up`.\nlisten: 127.0.0.1:7680\nservices:\n  {}\nalerts:\n  # session: x\nheartbeats: {}\n";
        let out = set_route(up, "pages", "http://10.0.0.5:7670", Some("/healthz"), &["*--*.apps".into()]);
        let v: Value = yaml_serde::from_str(&out).unwrap();
        assert_eq!(v["services"]["pages"]["url"], "http://10.0.0.5:7670");
        assert_eq!(v["services"]["pages"]["hosts"][0], "*--*.apps");
        assert!(v["heartbeats"].as_object().unwrap().is_empty());
        let again = set_route(&out, "pages", "http://10.0.0.6:7670", Some("/healthz"), &[]);
        let v: Value = yaml_serde::from_str(&again).unwrap();
        assert_eq!(v["services"]["pages"]["url"], "http://10.0.0.6:7670");
        assert_eq!(v["services"].as_object().unwrap().len(), 1);
        let ctl = "listen: 127.0.0.1:7680\nservices:\n  control: { url: \"http://127.0.0.1:7600\", health: /healthz }\n  other:\n    url: http://x\nalerts: {}\n";
        let v: Value =
            yaml_serde::from_str(&set_route(ctl, "control", "http://10.0.0.5:7600", Some("/healthz"), &[])).unwrap();
        assert_eq!(v["services"]["control"]["url"], "http://10.0.0.5:7600");
        assert_eq!(v["services"]["other"]["url"], "http://x");
        let v: Value = yaml_serde::from_str(&set_route("listen: 127.0.0.1:1\n", "a", "http://b", None, &[])).unwrap();
        assert_eq!(v["services"]["a"]["url"], "http://b");
    }

    #[test]
    fn public_addresses_are_told_apart_from_private_ones() {
        for ip in [
            "127.0.0.1",
            "10.20.7.50",
            "192.168.1.2",
            "172.20.0.1",
            "100.101.102.103",
            "169.254.1.1",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "2a01:4f8::1", "203.0.113.9"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(listen_ip("[2a01:4f8::1]:7600"), Some("2a01:4f8::1".parse().unwrap()));
        assert_eq!(listen_ip("10.0.0.5:7600"), Some("10.0.0.5".parse().unwrap()));
        assert_eq!(listen_ip("server.example:7600"), None);
    }

    #[test]
    fn addresses_are_worked_out() {
        assert_eq!(http_from_link("wss://c.example/v1/link"), "https://c.example");
        assert_eq!(http_from_link("ws://10.0.0.5:7600/v1/link"), "http://10.0.0.5:7600");
        assert_eq!(bracket("fd7a::1"), "[fd7a::1]");
        assert_eq!(instance_for(Path::new("/tmp/x y/server home")), "move-server-home");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
