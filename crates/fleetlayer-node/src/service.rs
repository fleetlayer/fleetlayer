//! `fleetlayer-node install` and `uninstall`: the node as a launchd agent (macOS) or a systemd user service (Linux).
//! The templates are `deploy/dev.fleetlayer.node.plist` and `deploy/fleetlayer-node.service`. First written for
//! Quietdeck by the same author.

use std::path::{Path, PathBuf};
use std::process::Command;

const PLIST: &str = include_str!("../../../deploy/dev.fleetlayer.node.plist");
const UNIT: &str = include_str!("../../../deploy/fleetlayer-node.service");
const LABEL: &str = "dev.fleetlayer.node";

pub struct Values {
    pub bin: PathBuf,
    pub path: String,
    pub log: PathBuf,
}

pub fn render(template: &str, v: &Values) -> String {
    template
        .replace("{{BIN}}", &v.bin.display().to_string())
        .replace("{{PATH}}", &v.path)
        .replace("{{LOG}}", &v.log.display().to_string())
}

fn service_file(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
    } else {
        home.join(".config/systemd/user/fleetlayer-node.service")
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

pub fn install(home: &Path, config: &Path, start: bool) -> anyhow::Result<()> {
    // Check the config now, or the service would restart forever on a typo.
    crate::config::NodeConfig::load(config)?;
    let values = Values {
        bin: std::env::current_exe()?.canonicalize()?,
        path: std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
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
        // Replace a loaded copy, so a reinstall picks up a new binary or PATH.
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
        run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
        println!("Started. Logs: {}", values.log.display());
    } else {
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", "fleetlayer-node.service"]))?;
        run(Command::new("systemctl").args(["--user", "restart", "fleetlayer-node.service"]))?;
        println!("Started. Logs: journalctl --user -u fleetlayer-node");
        println!("To keep it running while you're logged out: loginctl enable-linger \"$USER\"");
    }
    Ok(())
}

pub fn uninstall(home: &Path) -> anyhow::Result<()> {
    let file = service_file(home);
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", "fleetlayer-node.service"]).output();
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
