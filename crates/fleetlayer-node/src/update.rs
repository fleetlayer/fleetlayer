//! Self-update from Fleetlayer's GitHub releases.
//!
//! ```yaml
//! update:
//!   mode: auto        # auto | notify | off (default: auto when installed by the installer, else notify)
//!   every_hours: 6
//! ```
//!
//! Every `every_hours` the node reads the latest release. When it's newer than this binary, it downloads the
//! tarball for this platform and checks it against the release's `SHA256SUMS`, installs it, and exits so the service
//! manager starts the new version. It replaces only an install it made itself (binaries under
//! `~/.local/lib/fleetlayer/`): a node installed some other way (a package, Ansible) only logs that a release is out,
//! and its owner updates it.
//!
//! The install's layout, the same as `install.sh` makes:
//!
//! ```text
//! ~/.local/lib/fleetlayer/<binary>          the live binaries: each replaced by a rename, at a path that never changes
//! ~/.local/lib/fleetlayer/skills/           the live release's skills
//! ~/.local/lib/fleetlayer/VERSION           the live release's tag
//! ~/.local/lib/fleetlayer/versions/<tag>/   each release as it was unpacked, the newest two: copy one back to go back
//! ~/.local/bin/<binary>                     symlinks to the live binaries
//! ```
//!
//! The path matters on macOS, which files a privacy permission (Files and Folders, Local Network) under the real path
//! of the program that asked. With a folder per release, as before v0.3.13, every upgrade was a new program to macOS
//! and people were asked again; a signed build at an unchanged path keeps the answer (docs/releasing.md). A node
//! that starts from a folder of the old layout moves itself to this one, once.
//!
//! Not yet: verifying a signature made with a key the binary trusts. The checksum file comes from the same release
//! over HTTPS, and every release carries GitHub build attestations (`gh attestation verify`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Node;

const REPO: &str = "fleetlayer/fleetlayer";
pub const BINARIES: &[&str] = &[
    "fleetlayer",
    "fleetlayer-node",
    "fleetlayer-herdr",
    "fleetlayer-rex",
    "fleetlayer-claude-code",
    "fleetlayer-summaries",
    "fleetlayer-file-sync",
    "fleetlayer-claude-auth-manager",
    "fleetlayer-claude-accounts",
    "fleetlayer-control",
    "fleetlayer-gateway",
    "fleetlayer-pages",
    "fleetlayer-signin",
    "fleetlayer-shared-folders",
    "fleetlayer-scheduler",
    "fleetlayer-herdr-fleet",
    "fleetlayer-cookies",
    "fleetlayer-1password",
    "fleetlayer-egress",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Auto,
    #[default]
    Notify,
    Off,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub mode: Option<Mode>,
    #[serde(default = "six")]
    pub every_hours: u64,
}

fn six() -> u64 {
    6
}

/// This build's target triple, as the release's tarballs name it.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// `v0.2.10` → (0, 2, 10).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn lib_dir(home: &Path) -> PathBuf {
    home.join(".local/lib/fleetlayer")
}

/// Whether this binary was installed by the installer (it lives under `~/.local/lib/fleetlayer/`).
pub fn self_installed(home: &Path) -> bool {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .is_ok_and(|p| p.starts_with(lib_dir(home).canonicalize().unwrap_or_else(|_| lib_dir(home))))
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub async fn run(node: Arc<Node>, cfg: UpdateConfig) {
    let installed = self_installed(&node.home);
    if installed && let Some(old) = old_layout_dir(&node.home) {
        match adopt(&lib_dir(&node.home), &old, &node.home.join(".local/bin")) {
            Ok(()) => {
                tracing::info!(
                    from = %old.display(),
                    "moved this install to ~/.local/lib/fleetlayer, a path that stays the same across updates; restarting from there. On macOS, permissions given to the node are asked for once more"
                );
                // The service manager starts the binary at its new path, through the ~/.local/bin symlink.
                std::process::exit(0);
            }
            Err(e) => tracing::error!("moving this install to the layout that keeps one path: {e}"),
        }
    }
    // The node that installed this release linked only the binaries it knew of: link any this one ships that it
    // didn't (a new plugin or tool), so they're on PATH from the first start. Only for the installer's releases: a
    // node installed some other way (a package, Ansible) has its binaries put on PATH by that.
    if installed
        && let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(Path::to_path_buf))
        && let Err(e) = link_missing(&dir, &node.home.join(".local/bin"))
    {
        tracing::warn!("linking this release's binaries into ~/.local/bin: {e}");
    }
    let mode = cfg.mode.unwrap_or(if installed { Mode::Auto } else { Mode::Notify });
    if mode == Mode::Off {
        return;
    }
    let client = match reqwest::Client::builder()
        .user_agent(concat!("fleetlayer-node/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => return tracing::error!("update: {e}"),
    };
    tokio::time::sleep(Duration::from_secs(300)).await;
    loop {
        match check(&client).await {
            Ok(Some(release)) => {
                if mode == Mode::Auto && installed {
                    match install(&client, &node.home, &release).await {
                        Ok(()) => {
                            tracing::info!(version = %release.tag_name, "updated; restarting into the new version");
                            // The service manager starts the new binary.
                            std::process::exit(0);
                        }
                        Err(e) => tracing::error!(version = %release.tag_name, "update failed: {e}"),
                    }
                } else {
                    tracing::info!(
                        version = %release.tag_name,
                        "a newer Fleetlayer release is out; this node was installed some other way, so update it there"
                    );
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("update check: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(cfg.every_hours.max(1) * 3600)).await;
    }
}

/// The latest release, when it's newer than this binary.
async fn check(client: &reqwest::Client) -> anyhow::Result<Option<Release>> {
    let r: Release = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(newer(&r.tag_name, env!("CARGO_PKG_VERSION")).then_some(r))
}

async fn install(client: &reqwest::Client, home: &Path, release: &Release) -> anyhow::Result<()> {
    let target = target().ok_or_else(|| anyhow::anyhow!("no release is built for this platform"))?;
    let tarball = format!("fleetlayer-{}-{target}.tar.gz", release.tag_name);
    let asset = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| anyhow::anyhow!("the release has no {name}"))
    };
    let sums = client.get(asset("SHA256SUMS")?).send().await?.error_for_status()?.text().await?;
    let expected = sums
        .lines()
        .find_map(|l| l.split_once("  ").filter(|(_, n)| *n == tarball).map(|(h, _)| h.to_string()))
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS lists no {tarball}"))?;
    let bytes = client.get(asset(&tarball)?).send().await?.error_for_status()?.bytes().await?;
    let actual = hex::encode(Sha256::digest(&bytes));
    anyhow::ensure!(actual == expected, "{tarball}'s checksum is {actual}, and SHA256SUMS says {expected}");

    let lib = lib_dir(home);
    let staging = lib.join(format!(".staging-{}", release.tag_name));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let archive = staging.join(&tarball);
    std::fs::write(&archive, &bytes)?;
    let out = tokio::process::Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&staging).output().await?;
    anyhow::ensure!(out.status.success(), "tar: {}", String::from_utf8_lossy(&out.stderr).trim());
    let unpacked = staging.join(tarball.trim_end_matches(".tar.gz"));
    for b in BINARIES {
        anyhow::ensure!(unpacked.join(b).is_file(), "the release has no {b}");
    }
    let dest = lib.join("versions").join(&release.tag_name);
    std::fs::create_dir_all(lib.join("versions"))?;
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&unpacked, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    activate(&lib, &dest, &release.tag_name, &home.join(".local/bin"))
}

/// The folder this binary runs from, when it's a release folder of the layout before v0.3.13
/// (`~/.local/lib/fleetlayer/<tag>/`).
fn old_layout_dir(home: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    let lib = lib_dir(home).canonicalize().ok()?;
    (dir.parent()? == lib).then(|| dir.to_path_buf())
}

/// Make the release unpacked in `release` (`<lib>/versions/<tag>/`) the live one: copy each binary into `lib` under a
/// temporary name and rename it over the live file, so every binary keeps its path and is never half-written; swap
/// the skills folder; write `VERSION`; point `bin`'s symlinks at the live binaries; keep the newest two releases
/// under `versions/`.
pub fn activate(lib: &Path, release: &Path, tag: &str, bin: &Path) -> anyhow::Result<()> {
    for b in BINARIES {
        anyhow::ensure!(release.join(b).is_file(), "the release has no {b}");
    }
    for b in BINARIES {
        let tmp = lib.join(format!(".{b}.new"));
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(release.join(b), &tmp)?;
        std::fs::rename(&tmp, lib.join(b))?;
    }
    let (skills, new, old) = (lib.join("skills"), lib.join(".skills.new"), lib.join(".skills.old"));
    let _ = std::fs::remove_dir_all(&new);
    let _ = std::fs::remove_dir_all(&old);
    if release.join("skills").is_dir() {
        copy_dir(&release.join("skills"), &new)?;
    }
    if skills.symlink_metadata().is_ok() {
        std::fs::rename(&skills, &old)?;
    }
    if new.is_dir() {
        std::fs::rename(&new, &skills)?;
    }
    let _ = std::fs::remove_dir_all(&old);
    let tmp = lib.join(".VERSION.new");
    std::fs::write(&tmp, format!("{tag}\n"))?;
    std::fs::rename(&tmp, lib.join("VERSION"))?;
    link_binaries(lib, bin)?;
    prune_versions(&lib.join("versions"), tag);
    Ok(())
}

/// Copy a folder of plain files and folders. Anything else in it (a symbolic link) is left out.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (kind, dest) = (entry.file_type()?, to.join(entry.file_name()));
        if kind.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Keep the live release and the newest other one under `versions/`, and delete the rest.
fn prune_versions(versions: &Path, live: &str) {
    let Ok(entries) = std::fs::read_dir(versions) else { return };
    let mut others: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name() != live)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    others.sort();
    others.pop();
    for (_, dir) in others {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), "removing an old release: {e}");
        }
    }
}

/// Move an install of the layout before v0.3.13 to the one that keeps each binary at one path. The release in `old`
/// (the one running) becomes the live one first, so a failure after that leaves a node that starts. Then every
/// release folder in `lib` goes under `versions/`, and `old` itself becomes a symlink to `lib`, so a service file or
/// a link that names a binary in it finds the live binary.
pub fn adopt(lib: &Path, old: &Path, bin: &Path) -> anyhow::Result<()> {
    let tag = old
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} has no name", old.display()))?
        .to_string();
    activate(lib, old, &tag, bin)?;
    let versions = lib.join("versions");
    std::fs::create_dir_all(&versions)?;
    for entry in std::fs::read_dir(lib)?.flatten() {
        let name = entry.file_name();
        let release = entry.file_type().is_ok_and(|t| t.is_dir())
            && name != "versions"
            && name != "skills"
            && !name.to_string_lossy().starts_with('.')
            && entry.path().join("fleetlayer-node").is_file();
        if release {
            let dest = versions.join(&name);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(entry.path(), dest)?;
        }
    }
    std::os::unix::fs::symlink(".", old)?;
    prune_versions(&versions, &tag);
    Ok(())
}

/// Point `<bin>/<name>` at `<dir>/<name>` for every binary, each swapped atomically (a new symlink renamed over the
/// old one).
pub fn link_binaries(dir: &Path, bin: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(bin)?;
    for b in BINARIES {
        let tmp = bin.join(format!(".{b}.new"));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(dir.join(b), &tmp)?;
        std::fs::rename(&tmp, bin.join(b))?;
    }
    Ok(())
}

/// Link `<bin>/<name>` to `<dir>/<name>` for each binary `dir` has that `bin` has no link to yet (or a dangling one).
/// Leaves existing links alone: a person who pointed one elsewhere keeps it.
pub fn link_missing(dir: &Path, bin: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(bin)?;
    for b in BINARIES {
        let (src, link) = (dir.join(b), bin.join(b));
        if !src.is_file() || link.exists() {
            continue;
        }
        let tmp = bin.join(format!(".{b}.new"));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&src, &tmp)?;
        std::fs::rename(&tmp, &link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("v0.10.0", "0.9.0"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("v0.1.0", "0.2.0"));
        assert!(!newer("nightly", "0.1.0"));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
    }

    fn release(dir: &Path, content: &str, skill: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        for b in BINARIES {
            std::fs::write(dir.join(b), content).unwrap();
        }
        if let Some(s) = skill {
            std::fs::create_dir_all(dir.join("skills").join(s)).unwrap();
            std::fs::write(dir.join("skills").join(s).join("SKILL.md"), content).unwrap();
        }
    }

    #[test]
    fn an_update_replaces_the_binaries_at_the_same_path() {
        let d = tempfile::tempdir().unwrap();
        let (lib, bin) = (d.path().join("lib"), d.path().join("bin"));
        for (tag, skill) in [("v1", "one"), ("v2", "two"), ("v3", "two")] {
            release(&lib.join("versions").join(tag), tag, Some(skill));
            activate(&lib, &lib.join("versions").join(tag), tag, &bin).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let node = lib.join("fleetlayer-node");
        assert_eq!(std::fs::read_to_string(&node).unwrap(), "v3");
        assert_eq!(std::fs::read_link(bin.join("fleetlayer-node")).unwrap(), node, "the link names the live path");
        assert_eq!(std::fs::read_to_string(lib.join("VERSION")).unwrap(), "v3\n");
        assert!(lib.join("skills/two/SKILL.md").is_file());
        assert!(!lib.join("skills/one").exists(), "a skill the new release doesn't ship is gone");
        assert!(!lib.join("versions/v1").exists(), "only the live release and the one before are kept");
        assert!(lib.join("versions/v2/fleetlayer-node").is_file());
        assert!(lib.join("versions/v3/fleetlayer-node").is_file());
        let left: Vec<_> = std::fs::read_dir(&lib).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.iter().all(|n| !n.to_string_lossy().starts_with('.')), "no temporary file is left: {left:?}");
    }

    #[test]
    fn a_release_with_a_binary_missing_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (lib, bin) = (d.path().join("lib"), d.path().join("bin"));
        release(&lib.join("versions/v1"), "v1", None);
        activate(&lib, &lib.join("versions/v1"), "v1", &bin).unwrap();
        release(&lib.join("versions/v2"), "v2", None);
        std::fs::remove_file(lib.join("versions/v2/fleetlayer-cookies")).unwrap();
        assert!(activate(&lib, &lib.join("versions/v2"), "v2", &bin).is_err());
        assert_eq!(std::fs::read_to_string(lib.join("fleetlayer")).unwrap(), "v1");
        assert_eq!(std::fs::read_to_string(lib.join("VERSION")).unwrap(), "v1\n");
    }

    #[test]
    fn an_install_with_a_folder_per_release_moves_to_one_path() {
        let d = tempfile::tempdir().unwrap();
        let (lib, bin) = (d.path().join("lib"), d.path().join("bin"));
        release(&lib.join("v0.3.11"), "old", None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        release(&lib.join("v0.3.12"), "running", Some("fleetlayer"));
        link_binaries(&lib.join("v0.3.12"), &bin).unwrap();
        adopt(&lib, &lib.join("v0.3.12"), &bin).unwrap();
        assert_eq!(std::fs::read_to_string(lib.join("fleetlayer-node")).unwrap(), "running");
        assert_eq!(std::fs::read_link(bin.join("fleetlayer-node")).unwrap(), lib.join("fleetlayer-node"));
        assert_eq!(std::fs::read_to_string(lib.join("VERSION")).unwrap(), "v0.3.12\n");
        assert!(lib.join("versions/v0.3.12/fleetlayer-node").is_file());
        assert!(lib.join("versions/v0.3.11/fleetlayer-node").is_file());
        assert!(lib.join("skills/fleetlayer/SKILL.md").is_file());
        // A service file that names the old folder runs the live binary, at the live path.
        assert_eq!(
            lib.join("v0.3.12/fleetlayer-node").canonicalize().unwrap(),
            lib.join("fleetlayer-node").canonicalize().unwrap()
        );
        assert!(!lib.join("v0.3.11").exists());
    }

    #[test]
    fn symlinks_swap_to_the_new_version() {
        let d = tempfile::tempdir().unwrap();
        let (v1, v2, bin) = (d.path().join("v1"), d.path().join("v2"), d.path().join("bin"));
        for v in [&v1, &v2] {
            std::fs::create_dir_all(v).unwrap();
            for b in BINARIES {
                std::fs::write(v.join(b), "x").unwrap();
            }
        }
        link_binaries(&v1, &bin).unwrap();
        link_binaries(&v2, &bin).unwrap();
        assert_eq!(std::fs::read_link(bin.join("fleetlayer-node")).unwrap(), v2.join("fleetlayer-node"));
    }

    #[test]
    fn a_binary_the_old_node_didnt_link_is_linked_on_start() {
        let d = tempfile::tempdir().unwrap();
        let (v, bin) = (d.path().join("v2"), d.path().join("bin"));
        std::fs::create_dir_all(&v).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for b in BINARIES {
            std::fs::write(v.join(b), "x").unwrap();
        }
        std::fs::write(d.path().join("mine"), "x").unwrap();
        std::os::unix::fs::symlink(d.path().join("mine"), bin.join("fleetlayer")).unwrap();
        link_missing(&v, &bin).unwrap();
        assert_eq!(std::fs::read_link(bin.join("fleetlayer")).unwrap(), d.path().join("mine"), "left alone");
        assert_eq!(
            std::fs::read_link(bin.join("fleetlayer-claude-accounts")).unwrap(),
            v.join("fleetlayer-claude-accounts")
        );
    }

    #[test]
    fn this_platform_has_a_target() {
        assert!(target().is_some());
    }
}
