//! fleetlayer-file-sync: keep a directory the same on other nodes, such as skills or a folder of AGENTS.md files. The
//! publishing node seals each version to the targets' keys (`docs/protocol.md` section 8), so the control plane relays
//! it without being able to read it.
//!
//! ```yaml
//! - name: file-sync
//!   command: fleetlayer-file-sync
//!   grants: [sync.publish]
//!   config:
//!     publish:                       # on the source node
//!       - { set: skills, dir: ~/.claude/skills, to: [server, laptop], every_minutes: 5 }
//!     apply:                         # on each target node
//!       - { set: skills, dir: ~/.claude/skills }
//! ```
//!
//! Bundles are named `file-sync/<set>`. A version is published when the directory's contents change (checked every
//! `every_minutes`). Applying makes the target directory match: every file written, and files this plugin wrote for an
//! earlier version but missing now removed. A file it didn't write (listed in `.fleetlayer-files.json`, its record
//! of what it wrote) is never touched. Files are text, at most 1 MB each and 20 MB a set, and keep their executable bit.
//! A symbolic link pointing inside the directory is made as the same link on the target (a node too old for links
//! writes a copy of the file instead); one pointing outside, to nothing or to a hidden file is never followed. Anything
//! left out (not text, such a link, a socket) is a warning in the log and listed in the `file-sync.set` reading
//! (`{set, files, skipped, version, publishedAt}`, every check). A target that doesn't get a version is retried and
//! reported by the node (`sync.status` readings), so set it up with the targets upgraded first.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{
    self, NodeMeta, PluginMeta, Roles, SyncApply, SyncItem, SyncPublish, SyncRole, SyncTarget, meta_key,
};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const MAX_FILE: u64 = 1024 * 1024;
const MAX_SET: u64 = 20 * 1024 * 1024;
/// The record of the files this plugin wrote into a target folder.
const RECORD: &str = ".fleetlayer-file-sync.json";
/// Its name before the plugin was renamed from `files`: read when the new one is missing, removed once replaced.
const OLD_RECORD: &str = ".fleetlayer-files.json";

#[derive(Debug, Clone, Deserialize)]
struct Publish {
    set: String,
    dir: PathBuf,
    to: Vec<String>,
    #[serde(default = "five")]
    every_minutes: u64,
}

fn five() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
struct Apply {
    set: String,
    dir: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    publish: Vec<Publish>,
    #[serde(default)]
    apply: Vec<Apply>,
}

#[derive(Default)]
struct Plugin {
    apply: Mutex<Vec<Apply>>,
}

fn expand(home: &Path, p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// A set read from its folder: the items to send, and what was left out and why.
#[derive(Debug, Default)]
struct Read {
    items: BTreeMap<String, SyncItem>,
    skipped: Vec<String>,
}

/// Every text file and symbolic link under `dir`, by relative path (`/`-separated), skipping dot-files and the record.
/// A file keeps its executable bit. A link is carried as a link when it points inside `dir`, with the content of the
/// file it points to as well (so a node too old to make links writes a working copy); one pointing outside `dir`, to
/// nothing, or to a hidden file is skipped, never followed. Anything skipped is listed with the reason.
fn read_set(dir: &Path) -> anyhow::Result<Read> {
    let mut out = Read::default();
    let root = std::fs::canonicalize(dir)?;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let rel = e.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_symlink() {
                match link_inside(&root, &e.path(), &rel) {
                    Err(why) => out.skipped.push(format!("{rel} ({why})")),
                    Ok((link, resolved)) => {
                        let content = match std::fs::metadata(&resolved) {
                            Ok(m) if m.is_file() && m.len() <= MAX_FILE => {
                                total += m.len();
                                std::fs::read_to_string(&resolved).unwrap_or_default()
                            }
                            _ => String::new(),
                        };
                        out.items.insert(rel.clone(), SyncItem { name: rel, content, mode: None, link: Some(link) });
                    }
                }
            } else if ft.is_file() {
                let meta = e.metadata()?;
                let len = meta.len();
                anyhow::ensure!(len <= MAX_FILE, "{} is over 1 MB", e.path().display());
                total += len;
                match std::fs::read_to_string(e.path()) {
                    Ok(text) => {
                        let mode = executable_mode(&meta);
                        out.items.insert(rel.clone(), SyncItem { name: rel, content: text, mode, link: None });
                    }
                    Err(_) => out.skipped.push(format!("{rel} (not text)")),
                }
            } else {
                out.skipped.push(format!("{rel} (not a file, folder or link)"));
            }
            anyhow::ensure!(total <= MAX_SET, "{} is over 20 MB in all", dir.display());
        }
    }
    out.skipped.sort();
    Ok(out)
}

/// The permission bits of an executable file; None for one that isn't (the target's default).
fn executable_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    (mode & 0o111 != 0).then_some(mode)
}

/// The link at `path` (`rel` inside the set), as a relative link to where it really points, and that place, when it's
/// inside `root` (canonical) and not hidden; else why not.
fn link_inside(root: &Path, path: &Path, rel: &str) -> Result<(String, PathBuf), &'static str> {
    let resolved = std::fs::canonicalize(path).map_err(|_| "a link to nothing")?;
    let inside = resolved.strip_prefix(root).map_err(|_| "a link outside the folder")?;
    let inside = inside.to_string_lossy().replace('\\', "/");
    if !safe(&inside) {
        return Err("a link to a hidden file or the folder itself");
    }
    let depth = rel.matches('/').count();
    Ok((format!("{}{inside}", "../".repeat(depth)), resolved))
}

fn digest(set: &BTreeMap<String, SyncItem>) -> String {
    let mut h = Sha256::new();
    for (k, v) in set {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.content.as_bytes());
        h.update([0]);
        h.update(v.mode.map(|m| m.to_string()).unwrap_or_default().as_bytes());
        h.update([0]);
        h.update(v.link.as_deref().unwrap_or_default().as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

/// A relative path that stays inside the directory.
fn safe(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && rel.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.starts_with('.'))
}

/// A link at `name` pointing to `link` stays inside the directory (worked out from the names alone, so a link can't
/// be made to lead out, even through another link this plugin made, since those are checked the same way).
fn link_stays_inside(name: &str, link: &str) -> bool {
    if link.is_empty() || link.starts_with('/') {
        return false;
    }
    let mut at: Vec<&str> = name.split('/').collect();
    at.pop();
    for c in link.split('/') {
        match c {
            ".." => {
                if at.pop().is_none() {
                    return false;
                }
            }
            "" | "." => return false,
            c if c.starts_with('.') => return false,
            c => at.push(c),
        }
    }
    !at.is_empty()
}

/// No folder on the way to `name` under `dir` is a link (one made by hand could lead out of the directory).
fn no_link_on_the_way(dir: &Path, name: &str) -> bool {
    let mut p = dir.to_path_buf();
    let parts: Vec<&str> = name.split('/').collect();
    for c in &parts[..parts.len().saturating_sub(1)] {
        p.push(c);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
    }
    true
}

/// Make `dir` match `items`: write each file and make each link, remove the ones this plugin wrote before that `items`
/// no longer has.
fn apply_set(dir: &Path, items: &[SyncItem]) -> anyhow::Result<(usize, usize)> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let record_path = dir.join(RECORD);
    let old_record = dir.join(OLD_RECORD);
    let before: BTreeSet<String> = std::fs::read(&record_path)
        .or_else(|_| std::fs::read(&old_record))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut now = BTreeSet::new();
    for item in items {
        anyhow::ensure!(safe(&item.name), "refusing the path {:?}", item.name);
        if let Some(link) = &item.link {
            anyhow::ensure!(link_stays_inside(&item.name, link), "refusing the link {:?} -> {link:?}", item.name);
        }
        let path = dir.join(&item.name);
        if !no_link_on_the_way(dir, &item.name) {
            tracing::warn!("leaving {} alone: a folder on its way is a link", path.display());
            continue;
        }
        if std::fs::symlink_metadata(&path).is_ok() && !before.contains(&item.name) {
            tracing::warn!("leaving {} alone: it wasn't written by fleetlayer-file-sync", path.display());
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("fleetlayer-tmp");
        let _ = std::fs::remove_file(&tmp);
        match &item.link {
            Some(link) => std::os::unix::fs::symlink(link, &tmp)?,
            None => {
                std::fs::write(&tmp, &item.content)?;
                if let Some(mode) = item.mode {
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode & 0o777))?;
                }
            }
        }
        std::fs::rename(&tmp, &path)?;
        now.insert(item.name.clone());
    }
    let mut removed = 0;
    for old in before.difference(&now) {
        if safe(old) && std::fs::remove_file(dir.join(old)).is_ok() {
            removed += 1;
        }
    }
    std::fs::write(&record_path, serde_json::to_vec_pretty(&now)?)?;
    let _ = std::fs::remove_file(&old_record);
    Ok((now.len(), removed))
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-file-sync runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let apply: Vec<Apply> =
                    config.apply.iter().map(|a| Apply { set: a.set.clone(), dir: expand(&home, &a.dir) }).collect();
                let publish: Vec<Publish> =
                    config.publish.iter().map(|p| Publish { dir: expand(&home, &p.dir), ..p.clone() }).collect();
                let roles = Roles {
                    sync_target: (!apply.is_empty())
                        .then(|| SyncRole { bundles: apply.iter().map(|a| format!("file-sync/{}", a.set)).collect() }),
                    sync_source: (!publish.is_empty()).then(|| SyncRole {
                        bundles: publish.iter().map(|p| format!("file-sync/{}", p.set)).collect(),
                    }),
                    ..Default::default()
                };
                *self.0.apply.lock().await = apply;
                for p in publish {
                    tokio::spawn(watch(p, peer.clone()));
                }
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(
                            meta_key::PLUGIN,
                            &PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles },
                        )),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-file-sync".into(),
                        title: Some("Files".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SYNC_APPLY => {
                let a: SyncApply = params(p)?;
                let set = a.bundle.strip_prefix("file-sync/").unwrap_or_default();
                let dir = self
                    .0
                    .apply
                    .lock()
                    .await
                    .iter()
                    .find(|x| x.set == set)
                    .map(|x| x.dir.clone())
                    .ok_or_else(|| RpcError::not_here(format!("no apply entry for {}", a.bundle)))?;
                match apply_set(&dir, &a.items) {
                    Ok((written, removed)) => {
                        tracing::info!(set, version = a.version, written, removed, "applied");
                        Ok(json!({"applied": true, "written": written, "removed": removed}))
                    }
                    Err(e) => Ok(json!({"applied": false, "error": e.to_string()})),
                }
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Publish a version whenever the directory's contents change. Each check is a `file-sync.set` reading (what was sent,
/// what was skipped and why). A target that doesn't get a version is the node's to retry (and report, as a
/// `sync.status` reading), so a version counts as published once the node has it.
async fn watch(p: Publish, peer: Peer) {
    let mut last = String::new();
    let mut last_skipped: Vec<String> = vec![];
    let mut published: Option<(u64, String)> = None;
    tokio::time::sleep(Duration::from_secs(3)).await;
    while !peer.is_closed() {
        match read_set(&p.dir) {
            Ok(read) => {
                if read.skipped != last_skipped {
                    for s in read.skipped.iter().filter(|s| !last_skipped.contains(s)) {
                        tracing::warn!(set = %p.set, "not syncing {s}");
                    }
                    last_skipped = read.skipped.clone();
                }
                let d = digest(&read.items);
                let files = read.items.len();
                if d != last {
                    let version = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|t| t.as_millis() as u64)
                        .unwrap_or(0);
                    let publish = SyncPublish {
                        bundle: format!("file-sync/{}", p.set),
                        version,
                        secret: true,
                        targets: p.to.iter().map(|n| SyncTarget { node: n.clone() }).collect(),
                        items: read.items.into_values().collect(),
                    };
                    match peer
                        .request_within::<_, Value>(ext::method::SYNC_PUBLISH, &publish, Duration::from_secs(90))
                        .await
                    {
                        Ok(r) => {
                            let failed: Vec<&Value> =
                                r["results"].as_array().into_iter().flatten().filter(|x| x["ok"] != true).collect();
                            for f in &failed {
                                tracing::warn!(set = %p.set, version, target = %f["target"], error = %f["error"],
                                    "a target didn't get this version; the node tries it again");
                            }
                            tracing::info!(set = %p.set, version, failed = failed.len(), "published");
                            last = d;
                            published = Some((version, jiff::Timestamp::now().to_string()));
                        }
                        Err(e) => tracing::warn!(set = %p.set, "publishing: {e}"),
                    }
                }
                let reading = ext::Reading {
                    kind: "file-sync.set".into(),
                    key: p.set.clone(),
                    at: jiff::Timestamp::now(),
                    node: None,
                    data: json!({"set": p.set, "files": files, "skipped": last_skipped,
                        "version": published.as_ref().map(|x| x.0), "publishedAt": published.as_ref().map(|x| &x.1)}),
                };
                peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: vec![reading] });
            }
            Err(e) => tracing::warn!(set = %p.set, "reading {}: {e}", p.dir.display()),
        }
        tokio::time::sleep(Duration::from_secs(p.every_minutes.max(1) * 60)).await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, content: &str) -> SyncItem {
        SyncItem::text(name, content)
    }

    #[test]
    fn applying_matches_the_set_and_leaves_other_files_alone() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("mine.md"), "hand written").unwrap();
        apply_set(d.path(), &[item("a/SKILL.md", "one"), item("b.md", "two"), item("mine.md", "overwrite?")]).unwrap();
        assert_eq!(std::fs::read_to_string(d.path().join("a/SKILL.md")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(d.path().join("mine.md")).unwrap(), "hand written");
        let (_, removed) = apply_set(d.path(), &[item("a/SKILL.md", "one v2")]).unwrap();
        assert_eq!(removed, 1);
        assert!(!d.path().join("b.md").exists());
        assert!(d.path().join("mine.md").exists());
        assert!(apply_set(d.path(), &[item("../escape", "x")]).is_err());
    }

    #[test]
    fn a_target_written_before_the_rename_keeps_updating() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("SKILL.md"), "old").unwrap();
        std::fs::write(d.path().join(OLD_RECORD), r#"["SKILL.md"]"#).unwrap();
        apply_set(d.path(), &[item("SKILL.md", "new")]).unwrap();
        assert_eq!(std::fs::read_to_string(d.path().join("SKILL.md")).unwrap(), "new");
        assert!(d.path().join(RECORD).exists());
        assert!(!d.path().join(OLD_RECORD).exists());
    }

    #[test]
    fn a_set_reads_back_and_its_digest_follows_content() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("s")).unwrap();
        std::fs::write(d.path().join("s/SKILL.md"), "x").unwrap();
        std::fs::write(d.path().join(".hidden"), "y").unwrap();
        let a = read_set(d.path()).unwrap().items;
        assert_eq!(a.keys().collect::<Vec<_>>(), ["s/SKILL.md"]);
        std::fs::write(d.path().join("s/SKILL.md"), "z").unwrap();
        assert_ne!(digest(&a), digest(&read_set(d.path()).unwrap().items));
    }

    #[test]
    fn links_inside_and_executable_bits_travel_and_the_rest_is_listed_as_skipped() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        std::fs::create_dir_all(s.join("tool/bin")).unwrap();
        std::fs::write(s.join("tool/SKILL.md"), "skill").unwrap();
        std::fs::write(s.join("tool/bin/run"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(s.join("tool/bin/run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink("tool/SKILL.md", s.join("alias.md")).unwrap();
        symlink("..", s.join("tool/bin/up")).unwrap();
        symlink(s.join("tool/SKILL.md"), s.join("abs.md")).unwrap();
        symlink(outside.path().join("secret"), s.join("leak")).unwrap();
        symlink("nowhere", s.join("dangling")).unwrap();
        std::fs::write(s.join("blob"), [0xff, 0xfe, 0x00]).unwrap();
        let read = read_set(s).unwrap();
        assert_eq!(
            read.skipped,
            ["blob (not text)", "dangling (a link to nothing)", "leak (a link outside the folder)"]
        );
        let alias = &read.items["alias.md"];
        assert_eq!((alias.link.as_deref(), alias.content.as_str()), (Some("tool/SKILL.md"), "skill"));
        assert_eq!(read.items["abs.md"].link.as_deref(), Some("tool/SKILL.md"), "absolute links become relative");
        assert_eq!(read.items["tool/bin/up"].link.as_deref(), Some("../../tool"));
        assert_eq!(read.items["tool/bin/run"].mode, Some(0o755));
        assert_eq!(read.items["tool/SKILL.md"].mode, None);

        let dst = tempfile::tempdir().unwrap();
        let t = dst.path();
        apply_set(t, &read.items.values().cloned().collect::<Vec<_>>()).unwrap();
        assert_eq!(std::fs::read_link(t.join("alias.md")).unwrap(), Path::new("tool/SKILL.md"));
        assert_eq!(std::fs::read_to_string(t.join("alias.md")).unwrap(), "skill");
        assert_eq!(std::fs::read_to_string(t.join("tool/bin/up/SKILL.md")).unwrap(), "skill");
        let mode = std::fs::metadata(t.join("tool/bin/run")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        // A link the source removed goes; a version without links replaces them with files.
        apply_set(t, &[item("alias.md", "now a file")]).unwrap();
        assert!(!std::fs::symlink_metadata(t.join("alias.md")).unwrap().file_type().is_symlink());
        assert!(std::fs::symlink_metadata(t.join("tool/bin/up")).is_err());
    }

    #[test]
    fn a_link_that_would_lead_out_is_refused() {
        assert!(link_stays_inside("a/b", "../c"));
        assert!(link_stays_inside("a/b", "c/d"));
        assert!(!link_stays_inside("a/b", "../../c"));
        assert!(!link_stays_inside("a", "/etc/passwd"));
        assert!(!link_stays_inside("a", ".."));
        assert!(!link_stays_inside("a", "../.ssh/id"));
        let d = tempfile::tempdir().unwrap();
        let bad = SyncItem { name: "x".into(), content: String::new(), mode: None, link: Some("../../etc".into()) };
        assert!(apply_set(d.path(), &[bad]).is_err());
        // A folder on the way that's a link made by hand isn't written through.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("out")).unwrap();
        apply_set(d.path(), &[item("out/x.md", "x")]).unwrap();
        assert!(!outside.path().join("x.md").exists());
    }
}
