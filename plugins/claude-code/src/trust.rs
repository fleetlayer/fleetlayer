//! Getting a folder ready for a Claude Code session nobody is watching start.
//!
//! Claude Code asks "Do you trust the files in this folder?" before it takes input, and keeps the answer per folder in
//! `~/.claude.json` (`projects.<absolute path>.hasTrustDialogAccepted`). It asks about a folder even when a parent is
//! trusted, and never keeps the answer for the home folder, so a session started for an agent would wait at the prompt
//! forever. Before such a session starts, its exact folder is marked trusted here.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Mark `dir` trusted in the Claude Code config at `path`, keeping every other key. The file is replaced in one
/// rename, so Claude Code never reads half of it. A missing file is created.
pub fn mark_trusted(path: &Path, dir: &str) -> Result<(), String> {
    // Through a symlink to the file it names, so a linked config stays linked.
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut root: Value = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{} isn't JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let obj = root.as_object_mut().ok_or_else(|| format!("{} isn't a JSON object", path.display()))?;
    let projects = obj.entry("projects").or_insert_with(|| Value::Object(Map::new()));
    let projects = projects.as_object_mut().ok_or_else(|| format!("projects in {} isn't an object", path.display()))?;
    let project = projects.entry(dir).or_insert_with(|| Value::Object(Map::new()));
    let project =
        project.as_object_mut().ok_or_else(|| format!("projects.{dir} in {} isn't an object", path.display()))?;
    if project.get("hasTrustDialogAccepted") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    write_atomic(path, &serde_json::to_vec_pretty(&root).map_err(|e| e.to_string())?)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("claude.json");
    let tmp: PathBuf = dir.join(format!(".{name}.fleetlayer-{}", std::process::id()));
    // The same mode as the file it replaces (Claude Code's is 0600), 0600 for a new one.
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {e}", path.display())
    })
}

/// Where a session worked, from its transcript: the first record that names a `cwd`.
pub fn transcript_cwd(transcript: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(transcript).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .take(200)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .find_map(|v| v.get("cwd").and_then(Value::as_str).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_that_folder_is_marked_and_everything_else_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".claude.json");
        std::fs::write(&p, r#"{"numStartups":3,"projects":{"/srv":{"hasTrustDialogAccepted":true,"x":1}}}"#).unwrap();
        mark_trusted(&p, "/srv/app").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["projects"]["/srv"]["x"], 1);
        assert_eq!(v["projects"]["/srv/app"]["hasTrustDialogAccepted"], true);
        // A second time changes nothing; no temporary file is left behind.
        mark_trusted(&p, "/srv/app").unwrap();
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_missing_file_is_created_private_and_a_broken_one_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".claude.json");
        mark_trusted(&p, "/w").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&p, "{not json").unwrap();
        assert!(mark_trusted(&p, "/w").unwrap_err().contains("isn't JSON"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
    }

    #[test]
    fn a_transcript_names_its_folder() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("s.jsonl");
        std::fs::write(&t, "{\"type\":\"summary\"}\n{\"type\":\"user\",\"cwd\":\"/srv/app\"}\n").unwrap();
        assert_eq!(transcript_cwd(&t).as_deref(), Some("/srv/app"));
    }
}
