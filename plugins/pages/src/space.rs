//! Spaces: folders of the vault, each with an owner, holding one folder per page.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::render::{self, Kind};

/// The file that says a space's folder is really there: a share that has dropped looks like an empty folder.
pub const MARKER: &str = ".fleetlayer-space";

/// Fingerprinting stops after this many files: a page is a handful of files, not a tree.
const MAX_WALK: usize = 2000;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceConfig {
    pub dir: PathBuf,
    /// Shown in the list of spaces; the space's name when absent.
    #[serde(default)]
    pub title: Option<String>,
    /// The person whose space it is (`person:<name>`). Private to them unless the policy shares it. With the login
    /// key there's one reader, so it's informational until sign-in goes through Fleetlayer.
    #[serde(default)]
    pub owner: Option<String>,
    /// The tenant that owns the space, named in every access check once sign-in goes through Fleetlayer.
    #[serde(default)]
    pub tenant: Option<String>,
    /// The space's apps folder: each folder in it is an app on a subdomain of its own. No apps when absent.
    #[serde(default)]
    pub apps: Option<PathBuf>,
    /// The space's agent reads outside input (email, the web): its apps can't reach other sites, and opening one shows
    /// a notice first.
    #[serde(default)]
    pub untrusted: bool,
}

/// A space name: lower case letters, digits and hyphens, not starting with `_` or `.` (those paths are the app's).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 60
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
}

/// One entry in a space's list.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub folder: String,
    pub title: String,
    pub summary: Option<String>,
    /// `YYYY-MM-DD` from the folder name, else from the last change.
    pub date: String,
    pub modified: u64,
    pub kind: Kind,
    pub problems: usize,
}

/// Parsed pages by folder, kept while the folder's fingerprint stays the same.
#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<PathBuf, (String, Summary)>>,
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The date at the start of a folder name (`2026-11-27-…`).
pub fn folder_date(name: &str) -> Option<&str> {
    let d = name.get(..10)?;
    let b = d.as_bytes();
    let ok = b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    (ok && (name.len() == 10 || name.as_bytes()[10] == b'-')).then_some(d)
}

/// A day from seconds since 1970, in UTC.
pub fn day(secs: u64) -> String {
    jiff::Timestamp::from_second(secs as i64).map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// A fingerprint of everything under `dir` that's served (names, sizes, change times): it changes when a file does.
pub fn fingerprint(dir: &Path) -> String {
    let mut h = Sha256::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut n = 0;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            h.update(b"!");
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.starts_with('.') || s.starts_with('_') {
                continue;
            }
            n += 1;
            if n > MAX_WALK {
                return hex::encode(h.finalize());
            }
            let Ok(m) = e.metadata() else { continue };
            h.update(e.path().strip_prefix(dir).unwrap_or(&e.path()).to_string_lossy().as_bytes());
            h.update(m.len().to_le_bytes());
            let nanos = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
            h.update(nanos.to_le_bytes());
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    hex::encode(h.finalize())
}

/// The newest change time of anything in a page's folder.
fn newest(dir: &Path) -> u64 {
    let mut best = std::fs::metadata(dir).and_then(|m| m.modified()).map(secs).unwrap_or(0);
    let mut stack = vec![dir.to_path_buf()];
    let mut n = 0;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            n += 1;
            if n > MAX_WALK {
                return best;
            }
            if let Ok(m) = e.metadata() {
                best = best.max(m.modified().map(secs).unwrap_or(0));
                if m.is_dir() {
                    stack.push(e.path());
                }
            }
        }
    }
    best
}

/// Whether the space's folder is there and readable (its marker present).
pub fn check(name: &str, cfg: &SpaceConfig) -> Result<(), String> {
    check_dir(name, &cfg.dir)?;
    if let Some(apps) = &cfg.apps {
        check_dir(&format!("{name} (apps)"), apps)?;
    }
    Ok(())
}

fn check_dir(name: &str, dir: &Path) -> Result<(), String> {
    match std::fs::metadata(dir.join(MARKER)) {
        Ok(_) => match std::fs::read_dir(dir) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("space {name}: can't read {}: {e}", dir.display())),
        },
        Err(_) => Err(format!("space {name}: no {MARKER} in {} (the share may have dropped)", dir.display())),
    }
}

impl Cache {
    /// The space's pages, newest first.
    pub fn list(&self, cfg: &SpaceConfig) -> Vec<Summary> {
        let Ok(rd) = std::fs::read_dir(&cfg.dir) else { return vec![] };
        let mut out = vec![];
        let mut seen = vec![];
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with('_') || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let dir = e.path();
            let fp = fingerprint(&dir);
            seen.push(dir.clone());
            if let Some((f, s)) = self.entries.lock().unwrap().get(&dir)
                && *f == fp
            {
                out.push(s.clone());
                continue;
            }
            let r = render::page(&cfg.dir, Path::new(&name));
            let modified = newest(&dir);
            let s = Summary {
                title: r.title.clone().unwrap_or_else(|| name.clone()),
                summary: r.summary.clone(),
                date: folder_date(&name).map(str::to_string).unwrap_or_else(|| day(modified)),
                modified,
                kind: r.kind,
                problems: r.problems.len(),
                folder: name,
            };
            self.entries.lock().unwrap().insert(dir, (fp, s.clone()));
            out.push(s);
        }
        self.entries.lock().unwrap().retain(|k, _| !k.starts_with(&cfg.dir) || seen.contains(k));
        out.sort_by(|a, b| b.date.cmp(&a.date).then(b.modified.cmp(&a.modified)).then(a.folder.cmp(&b.folder)));
        out
    }
}

/// A file inside a space, refused when it's hidden, leaves the space (symbolic links included) or isn't there.
pub fn resolve(space_dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if !render::servable(rel) {
        return None;
    }
    let root = space_dir.canonicalize().ok()?;
    let full = root.join(rel).canonicalize().ok()?;
    full.starts_with(&root).then_some(full)
}

/// A folder name as a page name in a path segment: the same rules as space names plus dots and underscores inside.
pub fn valid_folder(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && !s.starts_with('_') && !s.contains('/') && !s.contains('\\') && s != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: &Path) -> SpaceConfig {
        SpaceConfig {
            dir: d.to_path_buf(),
            title: None,
            owner: Some("person:nathan".into()),
            tenant: None,
            apps: None,
            untrusted: false,
        }
    }

    #[test]
    fn folder_dates_are_read_from_the_name() {
        assert_eq!(folder_date("2026-11-27-qa"), Some("2026-11-27"));
        assert_eq!(folder_date("2026-11-27"), Some("2026-11-27"));
        assert_eq!(folder_date("2026-11-2x-qa"), None);
        assert_eq!(folder_date("2026-11-270"), None);
        assert_eq!(folder_date("qa"), None);
    }

    #[test]
    fn the_list_is_newest_first_and_skips_hidden_folders() {
        let d = tempfile::tempdir().unwrap();
        for (f, t) in [("2026-11-20-old", "Old"), ("2026-11-27-new", "New"), ("_draft", "Draft"), (".git", "x")] {
            std::fs::create_dir_all(d.path().join(f)).unwrap();
            std::fs::write(d.path().join(f).join("index.md"), format!("# {t}\n\nSummary of {t}.\n")).unwrap();
        }
        std::fs::write(d.path().join("loose.md"), "# not a page").unwrap();
        let c = Cache::default();
        let l = c.list(&cfg(d.path()));
        let titles: Vec<_> = l.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["New", "Old"]);
        assert_eq!(l[0].summary.as_deref(), Some("Summary of New."));
        assert_eq!(l[0].problems, 0);
    }

    #[test]
    fn the_list_notices_a_changed_page() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("p")).unwrap();
        std::fs::write(d.path().join("p/index.md"), "# One\n\nS.\n").unwrap();
        let c = Cache::default();
        assert_eq!(c.list(&cfg(d.path()))[0].title, "One");
        std::fs::write(d.path().join("p/index.md"), "# Two, longer\n\nS.\n").unwrap();
        assert_eq!(c.list(&cfg(d.path()))[0].title, "Two, longer");
    }

    #[test]
    fn a_missing_marker_fails_the_check() {
        let d = tempfile::tempdir().unwrap();
        assert!(check("s", &cfg(d.path())).unwrap_err().contains(MARKER));
        std::fs::write(d.path().join(MARKER), "").unwrap();
        assert!(check("s", &cfg(d.path())).is_ok());
    }

    #[test]
    fn files_resolve_only_inside_the_space() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        std::fs::create_dir_all(d.path().join("p")).unwrap();
        std::fs::write(d.path().join("p/a.jpg"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), d.path().join("p/link")).unwrap();
        assert!(resolve(d.path(), "p/a.jpg").is_some());
        assert!(resolve(d.path(), "p/link").is_none());
        assert!(resolve(d.path(), "../x").is_none());
        assert!(resolve(d.path(), ".fleetlayer-space").is_none());
        assert!(resolve(d.path(), "p/nope").is_none());
    }

    #[test]
    fn the_fingerprint_changes_with_a_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "1").unwrap();
        let a = fingerprint(d.path());
        assert_eq!(a, fingerprint(d.path()));
        std::fs::write(d.path().join("a"), "22").unwrap();
        assert_ne!(a, fingerprint(d.path()));
    }

    #[test]
    fn names() {
        assert!(valid_name("personal"));
        assert!(!valid_name("_app"));
        assert!(!valid_name("Personal"));
        assert!(valid_folder("2026-11-27-qa"));
        assert!(!valid_folder(".."));
        assert!(!valid_folder(".git"));
    }
}
