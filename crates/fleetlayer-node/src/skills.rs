//! The skills that ship with a release, installed where agents read them, so every agent on the machine knows what
//! Fleetlayer gives it (`docs/guides/agents-know-fleetlayer.md`).
//!
//! A release carries them in `skills/<name>/` beside the binaries. When the node starts, it copies each into Claude
//! Code's skills folder (`~/.claude/skills`, when `~/.claude` exists) and Codex's (`~/.agents/skills`, when `~/.agents`
//! or `~/.codex` exists), writing a `.fleetlayer` file holding the version into each folder it makes. It replaces only
//! folders with that file, and only when the version changed (an update restarts the node, so a new release's skills
//! arrive with it). A folder of the same name without the file is a person's own: it's left alone and listed in
//! `fleetlayer status`. Skills an earlier release installed that this one doesn't ship are removed.
//! It never writes through a symbolic link: not the skills folder, not the agent's folder above it, not a skill's
//! folder or its marker. `skills: false` in node.yaml turns all of it off.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The file that marks a skill folder as Fleetlayer's; it holds the version that wrote it.
pub const MARKER: &str = ".fleetlayer";

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Where the skills went (`~/.claude/skills`...).
    pub dirs: Vec<PathBuf>,
    /// Installed or updated now, as `<dir>/<name>`.
    pub written: Vec<String>,
    /// Folders of the same name that a person made, left alone.
    pub skipped: Vec<PathBuf>,
    /// Removed: an earlier release's skills this one doesn't ship.
    pub removed: Vec<String>,
    /// Why nothing was installed, when nothing was (turned off, no skills in this install, an error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The skills beside this binary (`<release>/skills`, through the `~/.local/bin` symlink), if it was installed from
/// a release.
pub fn shipped() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?.join("skills");
    dir.is_dir().then_some(dir)
}

/// The skills folders of the agents installed for this user.
pub fn targets(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if home.join(".claude").is_dir() {
        out.push(home.join(".claude/skills"));
    }
    if home.join(".agents").is_dir() || home.join(".codex").is_dir() {
        out.push(home.join(".agents/skills"));
    }
    out
}

/// Install at start, as node.yaml says (`skills:`, default on).
pub fn at_start(enabled: bool, home: &Path, version: &str) -> Report {
    if !enabled {
        return Report { note: Some("turned off in node.yaml (skills: false)".into()), ..Default::default() };
    }
    let Some(src) = shipped() else {
        return Report {
            note: Some("this install has no skills folder (not installed from a release)".into()),
            ..Default::default()
        };
    };
    let targets = targets(home);
    if targets.is_empty() {
        return Report {
            note: Some("no Claude Code or Codex folder in the home directory".into()),
            ..Default::default()
        };
    }
    let report = install(&src, &targets, version);
    if !report.written.is_empty() || !report.removed.is_empty() {
        tracing::info!(written = ?report.written, removed = ?report.removed, "skills installed");
    }
    for s in &report.skipped {
        tracing::warn!(dir = %s.display(), "a skill folder of that name is someone's own, so Fleetlayer's isn't installed");
    }
    if let Some(n) = &report.note {
        tracing::warn!("skills: {n}");
    }
    report
}

/// Copy every skill in `src` into each of `targets`.
pub fn install(src: &Path, targets: &[PathBuf], version: &str) -> Report {
    let mut report = Report { dirs: targets.to_vec(), ..Default::default() };
    let mut names: Vec<String> = match std::fs::read_dir(src) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().join("SKILL.md").is_file())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| valid_name(n))
            .collect(),
        Err(e) => {
            report.note = Some(format!("can't read {}: {e}", src.display()));
            return report;
        }
    };
    names.sort();
    let mut errors = vec![];
    for target in targets {
        // Never write through a link: the folder itself, and the agent's folder it's in, must be real folders.
        if let Some(link) = [target.parent(), Some(target.as_path())]
            .into_iter()
            .flatten()
            .find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        {
            errors.push(format!("{} is a symbolic link, so no skills are written there", link.display()));
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(target) {
            errors.push(format!("{}: {e}", target.display()));
            continue;
        }
        for name in &names {
            let dest = target.join(name);
            match ours(&dest) {
                Some(Some(v)) if v == version => {}
                Some(_) => match replace(&src.join(name), &dest, version) {
                    Ok(()) => report.written.push(format!("{}/{name}", target.display())),
                    Err(e) => errors.push(format!("{}: {e}", dest.display())),
                },
                None => report.skipped.push(dest),
            }
        }
        // An earlier release's skills that this one doesn't ship.
        for e in std::fs::read_dir(target).into_iter().flatten().flatten() {
            let Some(name) = e.file_name().to_str().map(String::from) else { continue };
            if !names.contains(&name) && matches!(ours(&e.path()), Some(Some(_))) {
                match std::fs::remove_dir_all(e.path()) {
                    Ok(()) => report.removed.push(format!("{}/{name}", target.display())),
                    Err(err) => errors.push(format!("{}: {err}", e.path().display())),
                }
            }
        }
    }
    if !errors.is_empty() {
        report.note = Some(errors.join("; "));
    }
    report
}

/// Whether `dest` may be written: `Some(None)` when it doesn't exist, `Some(Some(version))` when it's a folder
/// Fleetlayer wrote, `None` when it's someone's own (a folder without the marker, a file, a symbolic link).
fn ours(dest: &Path) -> Option<Option<String>> {
    match std::fs::symlink_metadata(dest) {
        Err(_) => Some(None),
        Ok(m) if m.is_dir() => {
            let marker = dest.join(MARKER);
            match std::fs::symlink_metadata(&marker) {
                Ok(mm) if mm.is_file() => {
                    Some(Some(std::fs::read_to_string(&marker).unwrap_or_default().trim().into()))
                }
                _ => None,
            }
        }
        Ok(_) => None,
    }
}

/// Write `src`'s files into a new folder beside `dest`, then swap it into place, so an agent never reads half a
/// skill.
fn replace(src: &Path, dest: &Path, version: &str) -> std::io::Result<()> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("skill");
    let tmp = parent.join(format!(".{name}.fleetlayer-new"));
    let _ = std::fs::remove_dir_all(&tmp);
    copy_dir(src, &tmp)?;
    std::fs::write(tmp.join(MARKER), format!("{version}\n"))?;
    let old = parent.join(format!(".{name}.fleetlayer-old"));
    let _ = std::fs::remove_dir_all(&old);
    if dest.exists() {
        std::fs::rename(dest, &old)?;
    }
    std::fs::rename(&tmp, dest)?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Regular files and folders only; links are skipped.
fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let t = e.file_type()?;
        if t.is_dir() {
            copy_dir(&e.path(), &dest.join(e.file_name()))?;
        } else if t.is_file() {
            std::fs::copy(e.path(), dest.join(e.file_name()))?;
        }
    }
    Ok(())
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn installs_updates_on_a_new_version_and_never_touches_a_persons_own() {
        let t = tempfile::tempdir().unwrap();
        let (src, target) = (t.path().join("src"), t.path().join("skills"));
        skill(&src, "fleetlayer", "v1");
        skill(&src, "pages", "v1");
        // The person's own pages skill, made by hand.
        skill(&target, "pages", "mine");

        let r = install(&src, std::slice::from_ref(&target), "0.3.9");
        assert_eq!(r.written, [format!("{}/fleetlayer", target.display())]);
        assert_eq!(r.skipped, [target.join("pages")]);
        assert_eq!(std::fs::read_to_string(target.join("fleetlayer/.fleetlayer")).unwrap(), "0.3.9\n");
        assert_eq!(std::fs::read_to_string(target.join("pages/SKILL.md")).unwrap(), "mine");

        // Same version: nothing written.
        assert!(install(&src, std::slice::from_ref(&target), "0.3.9").written.is_empty());

        // A new release: updated in place; a skill it no longer ships is removed; a person's folder stays.
        std::fs::write(src.join("fleetlayer/SKILL.md"), "v2").unwrap();
        skill(&src, "fleetlayer-browser", "v2");
        let r = install(&src, std::slice::from_ref(&target), "0.3.10");
        assert_eq!(r.written.len(), 2);
        assert_eq!(std::fs::read_to_string(target.join("fleetlayer/SKILL.md")).unwrap(), "v2");
        std::fs::remove_dir_all(src.join("fleetlayer-browser")).unwrap();
        let r = install(&src, std::slice::from_ref(&target), "0.3.11");
        assert_eq!(r.removed, [format!("{}/fleetlayer-browser", target.display())]);
        assert!(!target.join("fleetlayer-browser").exists());
        assert_eq!(std::fs::read_to_string(target.join("pages/SKILL.md")).unwrap(), "mine");
        // No temporary folders left behind.
        let hidden: Vec<_> = std::fs::read_dir(&target)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(hidden.is_empty());
    }

    #[test]
    fn a_link_or_a_file_of_that_name_is_someones_own() {
        let t = tempfile::tempdir().unwrap();
        let (src, target) = (t.path().join("src"), t.path().join("skills"));
        skill(&src, "fleetlayer", "v1");
        skill(&src, "apps", "v1");
        skill(t.path(), "elsewhere", "theirs");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(t.path().join("elsewhere"), target.join("fleetlayer")).unwrap();
        std::fs::write(target.join("apps"), "a file").unwrap();
        let r = install(&src, std::slice::from_ref(&target), "0.3.9");
        assert!(r.written.is_empty());
        assert_eq!(r.skipped.len(), 2);
        assert_eq!(std::fs::read_to_string(t.path().join("elsewhere/SKILL.md")).unwrap(), "theirs");
    }

    #[test]
    fn a_skills_folder_that_is_a_link_is_never_written_through() {
        let t = tempfile::tempdir().unwrap();
        let (src, elsewhere) = (t.path().join("src"), t.path().join("elsewhere"));
        skill(&src, "fleetlayer", "v1");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(t.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, t.path().join(".claude/skills")).unwrap();
        let r = install(&src, &[t.path().join(".claude/skills")], "0.3.9");
        assert!(r.written.is_empty());
        assert!(r.note.unwrap().contains("is a symbolic link"));
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none(), "nothing written where it points");
    }

    #[test]
    fn agents_folders_are_used_only_when_that_agent_is_installed() {
        let t = tempfile::tempdir().unwrap();
        assert!(targets(t.path()).is_empty());
        std::fs::create_dir(t.path().join(".codex")).unwrap();
        assert_eq!(targets(t.path()), [t.path().join(".agents/skills")]);
        std::fs::create_dir(t.path().join(".claude")).unwrap();
        assert_eq!(targets(t.path()), [t.path().join(".claude/skills"), t.path().join(".agents/skills")]);
    }
}
