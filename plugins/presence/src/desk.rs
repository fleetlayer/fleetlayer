//! The desk source: runs on the person's own computer and reports whether its screen is locked and how long since the
//! last keyboard or mouse input, as a `presence.desk` reading keyed by the person. The authority hears it from any
//! node and trusts it only from that person's desks.
//!
//! macOS needs no privacy permission for either: both come from `ioreg`, the I/O Registry that any user may read
//! (`HIDIdleTime` of `IOHIDSystem`, and `CGSSessionScreenIsLocked` in the console users of the registry's root).
//! Elsewhere, `command:` names a program that prints `{"locked": bool, "idleSeconds": n}`.

use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};

/// How often it looks.
pub const SAMPLE: Duration = Duration::from_secs(10);
/// It reports at least this often, so the authority can tell a sleeping machine (no reports) from an idle one.
pub const KEEPALIVE: Duration = Duration::from_secs(60);
/// Input after this long idle is reported at once: the person is back.
pub const BACK_AFTER: u64 = 60;
/// Failed looks in a row before the plugin says it can't do its job.
const FAILS_BEFORE_PROBLEM: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub locked: Option<bool>,
    pub idle_seconds: Option<u64>,
}

/// Whether a new sample must be reported now rather than at the next keepalive.
pub fn worth_reporting(prev: Option<Sample>, now: Sample) -> bool {
    let Some(prev) = prev else { return true };
    if prev.locked != now.locked {
        return true;
    }
    match (prev.idle_seconds, now.idle_seconds) {
        (Some(a), Some(b)) => a >= BACK_AFTER && b < BACK_AFTER,
        (a, b) => a.is_some() != b.is_some(),
    }
}

/// `HIDIdleTime` (nanoseconds) from `ioreg -c IOHIDSystem -d 4 -r -k HIDIdleTime`.
pub fn parse_idle(ioreg: &str) -> Option<u64> {
    ioreg.lines().find_map(|l| {
        let (_, v) = l.split_once("\"HIDIdleTime\" = ")?;
        v.trim().parse::<u64>().ok().map(|ns| ns / 1_000_000_000)
    })
}

/// Whether the console session of user `uid` is locked, from `ioreg -n Root -d 1`: its entry in `IOConsoleUsers`
/// carries `"CGSSessionScreenIsLocked"=Yes` while the screen is locked. No entry on the console (another user is
/// there, or the login window) counts as locked. None when there's no `IOConsoleUsers` line at all.
pub fn parse_locked(ioreg: &str, uid: u32) -> Option<bool> {
    let line = ioreg.lines().find(|l| l.contains("\"IOConsoleUsers\""))?;
    let me = format!("\"kCGSSessionUserIDKey\"={uid}");
    let entry = line.split("},{").find(|e| {
        e.split(',').any(|kv| kv.trim_start_matches(['(', '{', ' ']).trim_end_matches(['}', ')', ' ']) == me)
    });
    Some(match entry {
        None => true,
        Some(e) => e.contains("\"CGSSessionScreenIsLocked\"=Yes") || e.contains("\"kCGSSessionOnConsoleKey\"=No"),
    })
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn uid() -> u32 {
    run("id", &["-u"]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(u32::MAX)
}

/// One look, the macOS way.
fn sample_macos(uid: u32) -> Result<Sample, String> {
    let idle = parse_idle(&run("ioreg", &["-c", "IOHIDSystem", "-d", "4", "-r", "-k", "HIDIdleTime"])?)
        .ok_or("ioreg shows no HIDIdleTime")?;
    let locked = parse_locked(&run("ioreg", &["-n", "Root", "-d", "1"])?, uid);
    Ok(Sample { locked, idle_seconds: Some(idle) })
}

/// One look through the configured program.
fn sample_command(cmd: &str) -> Result<Sample, String> {
    let out = run("sh", &["-c", cmd])?;
    let v: Value = serde_json::from_str(out.trim()).map_err(|e| format!("{cmd} didn't print JSON: {e}"))?;
    Ok(Sample { locked: v["locked"].as_bool(), idle_seconds: v["idleSeconds"].as_u64() })
}

pub struct DeskConfig {
    pub person: String,
    pub command: Option<String>,
}

/// Report until the node goes away. `publish` sends one reading; `status` reports how it's doing.
pub async fn run_source(
    config: DeskConfig,
    is_closed: impl Fn() -> bool,
    publish: impl Fn(Value),
    status: impl Fn(Option<String>, Value),
) {
    let uid = uid();
    let macos = cfg!(target_os = "macos");
    let mut prev: Option<Sample> = None;
    let mut last_sent: Option<std::time::Instant> = None;
    let mut last_status: Option<std::time::Instant> = None;
    let mut fails = 0u32;
    let mut last_error = String::new();
    let mut last_ok: Option<Timestamp> = None;
    while !is_closed() {
        let looked = match (&config.command, macos) {
            (Some(c), _) => {
                let c = c.clone();
                tokio::task::spawn_blocking(move || sample_command(&c)).await.unwrap_or_else(|e| Err(e.to_string()))
            }
            (None, true) => {
                tokio::task::spawn_blocking(move || sample_macos(uid)).await.unwrap_or_else(|e| Err(e.to_string()))
            }
            (None, false) => Err("this desk source reads macOS; on another system set command: to a program that \
                                  prints {\"locked\": bool, \"idleSeconds\": n}"
                .into()),
        };
        match looked {
            Ok(s) => {
                fails = 0;
                last_ok = Some(Timestamp::now());
                let due = last_sent.is_none_or(|t| t.elapsed() >= KEEPALIVE);
                if due || worth_reporting(prev, s) {
                    publish(json!({"locked": s.locked, "idleSeconds": s.idle_seconds}));
                    last_sent = Some(std::time::Instant::now());
                }
                prev = Some(s);
            }
            Err(e) => {
                fails += 1;
                if e != last_error {
                    tracing::warn!("reading the desk: {e}");
                }
                last_error = e;
            }
        }
        if fails == 1 || fails == FAILS_BEFORE_PROBLEM || last_status.is_none_or(|t| t.elapsed() >= KEEPALIVE) {
            let problem = (fails >= FAILS_BEFORE_PROBLEM).then(|| format!("can't read the desk: {last_error}"));
            status(
                problem,
                json!({"person": config.person, "lastLook": last_ok, "locked": prev.and_then(|s| s.locked),
                    "idleSeconds": prev.and_then(|s| s.idle_seconds)}),
            );
            last_status = Some(std::time::Instant::now());
        }
        tokio::time::sleep(SAMPLE).await;
    }
}
