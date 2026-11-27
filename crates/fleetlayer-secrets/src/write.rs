//! Private files, written in one rename: what a plugin writes for tools (an env file, a token file, a cookie jar),
//! and the registry.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Write `content` to `path`, mode 0600, through a file beside it and one rename (a symbolic link at `path` is
/// replaced, never followed). A missing folder is made, mode 0700.
pub fn atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = path.parent().ok_or_else(|| std::io::Error::other("a path with no folder"))?;
    if !parent.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let tmp = parent.join(format!(
        ".{}.fleetlayer-tmp",
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Write `path` unless something is there that this plugin didn't write (`ours` lists what it wrote before). The
/// error says what to do.
pub fn owned(path: &Path, content: &str, ours: &[PathBuf], plugin: &str) -> Result<(), String> {
    if !ours.iter().any(|p| p == path) && std::fs::symlink_metadata(path).is_ok() {
        return Err(format!(
            "{} is there already and wasn't written by {plugin}; move it away to have it written",
            path.display()
        ));
    }
    atomic(path, content).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Remove the files this plugin wrote before (`before`) that it no longer writes (`now`). Answers the ones it couldn't
/// remove, which it still owns.
pub fn remove_stale(before: &[PathBuf], now: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut failed = vec![];
    for p in before.iter().filter(|p| !now.contains(p)) {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => failed.push((p.clone(), e.to_string())),
            _ => {}
        }
    }
    failed
}

/// A value as a shell and dotenv readers take it: bare when it's plain, else in single quotes.
pub fn env_value(v: &str) -> String {
    let plain = !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "_-.%:/+=~@,".contains(c));
    if plain { v.to_string() } else { format!("'{}'", v.replace('\'', r"'\''")) }
}

/// An environment variable name: a letter or `_`, then letters, digits and `_`.
pub fn env_var_ok(v: &str) -> bool {
    let mut cs = v.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_writes_are_private_and_replace_links_rather_than_follow_them() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new/dir/token");
        atomic(&p, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let elsewhere = d.path().join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        atomic(&link, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "two");
    }

    #[test]
    fn a_persons_file_is_never_overwritten_and_stale_files_go() {
        let d = tempfile::tempdir().unwrap();
        let mine = d.path().join("mine.env");
        std::fs::write(&mine, "a person's").unwrap();
        let e = owned(&mine, "x", &[], "fleetlayer-1password").unwrap_err();
        assert!(e.contains("wasn't written by fleetlayer-1password"), "{e}");
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "a person's");
        owned(&mine, "x", std::slice::from_ref(&mine), "p").unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "x");
        let other = d.path().join("other");
        atomic(&other, "y").unwrap();
        assert!(remove_stale(&[mine.clone(), other.clone()], std::slice::from_ref(&mine)).is_empty());
        assert!(mine.exists() && !other.exists());
    }

    #[test]
    fn env_values_are_quoted_when_a_shell_needs_it() {
        assert_eq!(env_value("abc-123"), "abc-123");
        assert_eq!(env_value("it's"), r"'it'\''s'");
        assert_eq!(env_value(""), "''");
        assert!(env_var_ok("GITHUB_TOKEN") && !env_var_ok("1X") && !env_var_ok("A-B"));
    }
}
