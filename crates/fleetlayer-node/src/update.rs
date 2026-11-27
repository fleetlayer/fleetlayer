//! Self-update from Fleetlayer's GitHub releases.
//!
//! ```yaml
//! update:
//!   mode: auto        # auto | notify | off (default: auto when installed by the installer, else notify)
//!   every_hours: 6
//! ```
//!
//! Every `every_hours` the node reads the latest release. When it's newer than this binary, it downloads the
//! tarball for this platform and checks it against the release's `SHA256SUMS`, unpacks it into
//! `~/.local/lib/fleetlayer/<version>/`, points the symlinks in `~/.local/bin` at the new binaries (each swapped
//! atomically), and exits so the service manager starts the new version. It replaces only an install it made itself
//! (binaries under `~/.local/lib/fleetlayer/`): a node installed some other way (a package, Ansible) only logs that a
//! release is out, and its owner updates it.
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
    "fleetlayer-cookies",
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
    let dest = lib.join(&release.tag_name);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&unpacked, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    link_binaries(&dest, &home.join(".local/bin"))
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
