//! `fleetlayer-node install` and `uninstall`: the node as a launchd agent (macOS) or a systemd user service (Linux).
//! The templates are `deploy/dev.fleetlayer.node.plist` and `deploy/fleetlayer-node.service`. First written for
//! Quietdeck by the same author.

use std::path::{Path, PathBuf};
use std::process::Command;

const PLIST: &str = include_str!("../../../deploy/dev.fleetlayer.node.plist");
const UNIT: &str = include_str!("../../../deploy/fleetlayer-node.service");
const LABEL: &str = "dev.fleetlayer.node";

/// The launchd label and systemd unit: `dev.fleetlayer.node` and `fleetlayer-node.service`, or with
/// `$FLEETLAYER_INSTANCE` set, `dev.fleetlayer.<instance>.node` and `fleetlayer-<instance>-node.service`.
fn label() -> String {
    fleetlayer_protocol::local::launchd_label("node", fleetlayer_protocol::local::instance().as_deref())
}

fn unit_name() -> String {
    fleetlayer_protocol::local::systemd_unit("node", fleetlayer_protocol::local::instance().as_deref())
}

pub struct Values {
    pub bin: PathBuf,
    pub path: String,
    pub log: PathBuf,
}

pub fn render(template: &str, v: &Values) -> String {
    let mut out = template
        .replace("{{BIN}}", &v.bin.display().to_string())
        .replace("{{PATH}}", &v.path)
        .replace("{{LOG}}", &v.log.display().to_string());
    // A second instance names itself and carries its own HOME, which the service manager would otherwise set to the
    // account's.
    if let Some(i) = fleetlayer_protocol::local::instance() {
        let home = std::env::var("HOME").unwrap_or_default();
        out = out.replace(LABEL, &label());
        if out.contains("<key>PATH</key>") {
            let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            out = out.replacen(
                "    <key>PATH</key>",
                &format!(
                    "    <key>HOME</key>\n    <string>{}</string>\n    <key>FLEETLAYER_INSTANCE</key>\n    <string>{i}</string>\n    <key>PATH</key>",
                    esc(&home)
                ),
                1,
            );
        } else {
            out = out.replacen(
                "Environment=PATH=",
                &format!("Environment=\"HOME={home}\"\nEnvironment=FLEETLAYER_INSTANCE={i}\nEnvironment=PATH="),
                1,
            );
        }
    }
    out
}

fn service_file(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents").join(format!("{}.plist", label()))
    } else {
        home.join(".config/systemd/user").join(unit_name())
    }
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!("{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn uid() -> String {
    nix::unistd::getuid().to_string()
}

/// The path the service should run: the `fleetlayer-node` on PATH (a symlink an upgrade repoints, such as
/// `~/.local/bin/fleetlayer-node`) when it is this binary, so an upgrade that swaps symlinks reaches the service; else
/// this binary's own path.
fn stable_bin() -> anyhow::Result<PathBuf> {
    let me = std::env::current_exe()?.canonicalize()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("fleetlayer-node");
        if candidate.canonicalize().is_ok_and(|c| c == me) {
            return Ok(candidate);
        }
    }
    Ok(me)
}

pub fn install(home: &Path, config: &Path, start: bool) -> anyhow::Result<()> {
    // Check the config now, or the service would restart forever on a typo.
    crate::config::NodeConfig::load(config)?;
    let values = Values {
        bin: stable_bin()?,
        path: fleetlayer_protocol::local::service_path(
            &std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
            fleetlayer_protocol::local::instance().as_deref(),
            home,
            nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten().map(|u| u.dir).as_deref(),
        ),
        log: home.join("Library/Logs/fleetlayer-node.log"),
    };
    let file = service_file(home);
    std::fs::create_dir_all(file.parent().expect("a service file has a directory"))?;
    let template = if cfg!(target_os = "macos") { PLIST } else { UNIT };
    std::fs::write(&file, render(template, &values))?;
    println!("Wrote {}.", file.display());
    if !start {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        let service = format!("{domain}/{}", label());
        // Replace a loaded copy, so a reinstall picks up a new binary or PATH. bootout returns before launchd has
        // finished unloading, and bootstrapping too soon fails with "5: Input/output error": wait until it's gone
        // (up to 10 s), and retry the bootstrap once.
        let _ = Command::new("launchctl").args(["bootout", &service]).output();
        for _ in 0..20 {
            let loaded = Command::new("launchctl").args(["print", &service]).output().is_ok_and(|o| o.status.success());
            if !loaded {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file)).is_err() {
            std::thread::sleep(std::time::Duration::from_secs(3));
            run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
        }
        println!("Started. Logs: {}", values.log.display());
    } else {
        // systemd reads user units only from the account's own folders, so a second instance's file, under its own
        // HOME, is linked in.
        if fleetlayer_protocol::local::instance().is_some() {
            run(Command::new("systemctl").args(["--user", "link", "--force"]).arg(&file))?;
        }
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", &unit_name()]))?;
        run(Command::new("systemctl").args(["--user", "restart", &unit_name()]))?;
        println!("Started. Logs: journalctl --user -u {}", unit_name());
        println!("To keep it running while you're logged out: loginctl enable-linger \"$USER\"");
    }
    Ok(())
}

pub fn uninstall(home: &Path) -> anyhow::Result<()> {
    let file = service_file(home);
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", uid(), label())]).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit_name()]).output();
    }
    match std::fs::remove_file(&file) {
        Ok(()) => println!("Stopped and removed {}.", file.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Not installed ({} doesn't exist).", file.display())
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_every_placeholder() {
        let v =
            Values { bin: "/opt/fl/fleetlayer-node".into(), path: "/usr/bin:/bin".into(), log: "/tmp/n.log".into() };
        for t in [PLIST, UNIT] {
            let out = render(t, &v);
            assert!(!out.contains("{{"), "{out}");
            assert!(out.contains("/opt/fl/fleetlayer-node"));
            assert!(out.contains("/usr/bin:/bin"));
        }
        assert!(render(PLIST, &v).contains("<string>/tmp/n.log</string>"));
    }
}
