//! Starting and stopping a component where it runs. Normally as the user services `fleetlayer up` makes (launchd,
//! systemd --user). With `FLEETLAYER_MOVE_SERVICES=process`, as plain background processes with a pid file under
//! `~/.local/state/fleetlayer/run/`: how the tests run a whole move on one machine, and a way to try it without
//! touching the service manager.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::component::Kind;
use crate::up;

fn process_mode() -> bool {
    std::env::var("FLEETLAYER_MOVE_SERVICES").is_ok_and(|v| v == "process")
}

/// What runs a component: the control plane, the gateway, or the node (which runs every plugin).
fn unit_of(kind: &Kind) -> &'static str {
    match kind {
        Kind::Control => "control",
        Kind::Gateway => "gateway",
        Kind::Node | Kind::Plugin(_) => "node",
    }
}

fn run_dir() -> PathBuf {
    up::state_dir().join("run")
}

fn program(unit: &str) -> anyhow::Result<Vec<String>> {
    let l = up::Layout { config: up::config_dir(), state: up::state_dir() };
    Ok(match unit {
        "control" => vec![
            up::find_bin("fleetlayer-control")?.display().to_string(),
            "--config".into(),
            l.control_yaml().display().to_string(),
            "serve".into(),
        ],
        "gateway" => vec![
            up::find_bin("fleetlayer-gateway")?.display().to_string(),
            "--config".into(),
            l.gateway_yaml().display().to_string(),
            "serve".into(),
        ],
        _ => vec![
            up::find_bin("fleetlayer-node")?.display().to_string(),
            "--config".into(),
            l.node_yaml().display().to_string(),
            "run".into(),
        ],
    })
}

fn pid_of(unit: &str) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(run_dir().join(format!("{unit}.pid"))).ok()?.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Start (or restart) what runs `kind` here.
pub fn start(kind: &Kind) -> anyhow::Result<()> {
    let unit = unit_of(kind);
    if process_mode() {
        stop_unit(unit)?;
        use std::os::unix::process::CommandExt;
        up::private_dir(&run_dir())?;
        let args = program(unit)?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(run_dir().join(format!("{unit}.log")))?;
        let child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()?;
        std::fs::write(run_dir().join(format!("{unit}.pid")), child.id().to_string())?;
        return Ok(());
    }
    match unit {
        "control" => up::control_unit(
            &up::find_bin("fleetlayer-control")?,
            &up::Layout { config: up::config_dir(), state: up::state_dir() }.control_yaml(),
        )
        .install(),
        "gateway" => {
            up::gateway_unit(&up::find_bin("fleetlayer-gateway")?, &up::config_dir().join("gateway.yaml")).install()
        }
        _ => {
            up::run(Command::new(up::find_bin("fleetlayer-node")?).arg("install"))?;
            Ok(())
        }
    }
}

/// Stop what runs `kind` here, and keep it from starting again (its service file is removed; the files it uses stay).
pub fn stop(kind: &Kind) -> anyhow::Result<()> {
    stop_unit(unit_of(kind))
}

fn stop_unit(unit: &str) -> anyhow::Result<()> {
    if process_mode() {
        if let Some(pid) = pid_of(unit) {
            let p = nix::unistd::Pid::from_raw(pid);
            let _ = nix::sys::signal::kill(p, nix::sys::signal::Signal::SIGTERM);
            let until = Instant::now() + Duration::from_secs(15);
            while alive(pid) && Instant::now() < until {
                // Reap it if it's our child (a test that started it through us); otherwise just wait.
                let _ = nix::sys::wait::waitpid(p, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
                std::thread::sleep(Duration::from_millis(100));
            }
            if alive(pid) {
                let _ = nix::sys::signal::kill(p, nix::sys::signal::Signal::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(run_dir().join(format!("{unit}.pid")));
        return Ok(());
    }
    match unit {
        "node" => {
            let _ = Command::new(up::find_bin("fleetlayer-node")?).arg("uninstall").output()?;
        }
        _ => {
            let description = if unit == "control" { "Fleetlayer control plane" } else { "Fleetlayer gateway" };
            let name = if unit == "control" { "control" } else { "gateway" };
            up::Unit { name, description, args: vec![] }.uninstall()?;
        }
    }
    Ok(())
}

/// Whether what runs `kind` is running here (as far as this mode can tell).
pub fn running(kind: &Kind) -> bool {
    let unit = unit_of(kind);
    if process_mode() {
        return pid_of(unit).is_some();
    }
    if cfg!(target_os = "macos") {
        let label = fleetlayer_protocol::local::launchd_label(unit, fleetlayer_protocol::local::instance().as_deref());
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{label}", up::uid())])
            .output()
            .is_ok_and(|o| o.status.success())
    } else {
        let u = fleetlayer_protocol::local::systemd_unit(unit, fleetlayer_protocol::local::instance().as_deref());
        Command::new("systemctl").args(["--user", "is-active", "--quiet", &u]).status().is_ok_and(|s| s.success())
    }
}
