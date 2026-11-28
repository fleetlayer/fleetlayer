//! Small things every harness needs: replacing a file in one rename, editing a JSON settings file without losing a
//! key, telling whether a process is alive, and finding the agent process that ran a hook.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Replace `path` with `bytes` in one rename (the reader never sees half a file), keeping the old file's mode (0600
/// for a new one). Through a symlink to the file it names, so a linked or synced settings file stays linked.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp: PathBuf = dir.join(format!(".{name}.fleetlayer-{}", std::process::id()));
    let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {e}", path.display())
    })
}

/// Read a JSON object from `path` (an empty object when it doesn't exist), let `change` edit it, and write it back
/// only if it changed. True when the file changed. A file that isn't a JSON object is never touched (Gemini CLI allows
/// comments in its settings: such a file is left for the person).
pub fn edit_json(
    path: &Path,
    change: impl FnOnce(&mut Map<String, Value>) -> Result<(), String>,
) -> Result<bool, String> {
    let before: Value = match std::fs::read(path) {
        Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Value::Object(Map::new()),
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| format!("{} isn't plain JSON ({e}): left as it is", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let mut after = before.clone();
    let root = after.as_object_mut().ok_or_else(|| format!("{} isn't a JSON object", path.display()))?;
    change(root)?;
    if after == before {
        return Ok(false);
    }
    write_atomic(path, &serde_json::to_vec_pretty(&after).map_err(|e| e.to_string())?)?;
    Ok(true)
}

/// Write `contents` to `path` unless it's already exactly that. True when it wrote.
pub fn ensure_file(path: &Path, contents: &str) -> Result<bool, String> {
    if std::fs::read_to_string(path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    write_atomic(path, contents.as_bytes())?;
    Ok(true)
}

/// Remove `path` if it's there. True when it was.
pub fn remove(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("removing {}: {e}", path.display())),
    }
}

pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    if pid <= 1 {
        return false;
    }
    // Signal 0 checks the process exists; EPERM means it exists but isn't ours.
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(e) => e == nix::errno::Errno::EPERM,
    }
}

/// The agent process that ran this hook: the nearest ancestor that isn't a shell (an agent runs a hook's command
/// through `sh -c`). `ps` answers on Linux and macOS alike.
pub fn agent_pid() -> Option<u32> {
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..4 {
        let out = std::process::Command::new("ps").args(["-o", "ppid=,comm=", "-p", &pid.to_string()]).output().ok()?;
        let line = String::from_utf8_lossy(&out.stdout);
        let mut words = line.split_whitespace();
        let ppid: u32 = words.next()?.parse().ok()?;
        let comm = words.collect::<Vec<_>>().join(" ");
        let name = comm.rsplit('/').next().unwrap_or("").trim_start_matches('-');
        if !matches!(name, "sh" | "bash" | "dash" | "zsh" | "fish" | "ksh") {
            return Some(pid);
        }
        pid = ppid;
    }
    None
}

/// Fleetlayer's folder for a harness: `~/.local/state/fleetlayer/harness/<harness>`, 0700. The shims inside the agents
/// compute the same path, so it never depends on the environment the node and the agent each see.
pub fn state_dir(home: &Path, harness: &str) -> PathBuf {
    home.join(".local/state/fleetlayer/harness").join(harness)
}

pub fn make_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_every_other_key_and_skip_files_that_arent_json() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, r#"{"a":1,"b":{"c":2}}"#).unwrap();
        assert!(
            edit_json(&p, |r| {
                r.insert("x".into(), Value::Bool(true));
                Ok(())
            })
            .unwrap()
        );
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!((v["a"].clone(), v["b"]["c"].clone(), v["x"].clone()), (1.into(), 2.into(), true.into()));
        assert!(!edit_json(&p, |_| Ok(())).unwrap(), "nothing changed, nothing written");
        std::fs::write(&p, "{ // a comment\n}").unwrap();
        assert!(edit_json(&p, |_| Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ // a comment\n}");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "no temporary file left");
    }

    #[test]
    fn our_own_process_is_alive_and_nonsense_isnt() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }
}
