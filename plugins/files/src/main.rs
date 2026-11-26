//! fleetlayer-files: keep a directory the same on other nodes, such as skills or a folder of AGENTS.md files. The
//! publishing node seals each version to the targets' keys (`docs/protocol.md` section 8), so the control plane relays
//! it without being able to read it.
//!
//! ```yaml
//! - name: files
//!   command: fleetlayer-files
//!   grants: [sync.publish]
//!   config:
//!     publish:                       # on the source node
//!       - { set: skills, dir: ~/.claude/skills, to: [server, laptop], every_minutes: 5 }
//!     apply:                         # on each target node
//!       - { set: skills, dir: ~/.claude/skills }
//! ```
//!
//! Bundles are named `files/<set>`. A version is published when the directory's contents change (checked every
//! `every_minutes`). Applying makes the target directory match: every file written, and files this plugin wrote for an
//! earlier version but missing now removed. A file it didn't write (listed in `.fleetlayer-files.json`, its record
//! of what it wrote) is never touched. Files are text, at most 1 MB each and 20 MB a set.

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
const RECORD: &str = ".fleetlayer-files.json";

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

/// Every regular text file under `dir`, by relative path (`/`-separated), skipping dot-files and the record.
fn read_set(dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                let len = e.metadata()?.len();
                anyhow::ensure!(len <= MAX_FILE, "{} is over 1 MB", e.path().display());
                total += len;
                anyhow::ensure!(total <= MAX_SET, "{} is over 20 MB in all", dir.display());
                let rel = e.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                match std::fs::read_to_string(e.path()) {
                    Ok(text) => {
                        out.insert(rel, text);
                    }
                    Err(_) => tracing::warn!("skipping {} (not text)", e.path().display()),
                }
            }
        }
    }
    Ok(out)
}

fn digest(set: &BTreeMap<String, String>) -> String {
    let mut h = Sha256::new();
    for (k, v) in set {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
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

/// Make `dir` match `items`: write each file, remove the ones this plugin wrote before that `items` no longer has.
fn apply_set(dir: &Path, items: &[SyncItem]) -> anyhow::Result<(usize, usize)> {
    std::fs::create_dir_all(dir)?;
    let record_path = dir.join(RECORD);
    let before: BTreeSet<String> =
        std::fs::read(&record_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut now = BTreeSet::new();
    for item in items {
        anyhow::ensure!(safe(&item.name), "refusing the path {:?}", item.name);
        let path = dir.join(&item.name);
        if path.exists() && !before.contains(&item.name) {
            tracing::warn!("leaving {} alone: it wasn't written by fleetlayer-files", path.display());
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("fleetlayer-tmp");
        std::fs::write(&tmp, &item.content)?;
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-files runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let apply: Vec<Apply> =
                    config.apply.iter().map(|a| Apply { set: a.set.clone(), dir: expand(&home, &a.dir) }).collect();
                let publish: Vec<Publish> =
                    config.publish.iter().map(|p| Publish { dir: expand(&home, &p.dir), ..p.clone() }).collect();
                let roles = Roles {
                    sync_target: (!apply.is_empty())
                        .then(|| SyncRole { bundles: apply.iter().map(|a| format!("files/{}", a.set)).collect() }),
                    sync_source: (!publish.is_empty())
                        .then(|| SyncRole { bundles: publish.iter().map(|p| format!("files/{}", p.set)).collect() }),
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
                        name: "fleetlayer-files".into(),
                        title: Some("Files".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SYNC_APPLY => {
                let a: SyncApply = params(p)?;
                let set = a.bundle.strip_prefix("files/").unwrap_or_default();
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

/// Publish a version whenever the directory's contents change.
async fn watch(p: Publish, peer: Peer) {
    let mut last = String::new();
    tokio::time::sleep(Duration::from_secs(3)).await;
    while !peer.is_closed() {
        match read_set(&p.dir) {
            Ok(set) => {
                let d = digest(&set);
                if d != last {
                    let version = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|t| t.as_millis() as u64)
                        .unwrap_or(0);
                    let publish = SyncPublish {
                        bundle: format!("files/{}", p.set),
                        version,
                        secret: true,
                        targets: p.to.iter().map(|n| SyncTarget { node: n.clone() }).collect(),
                        items: set.into_iter().map(|(name, content)| SyncItem { name, content }).collect(),
                    };
                    match peer
                        .request_within::<_, Value>(ext::method::SYNC_PUBLISH, &publish, Duration::from_secs(90))
                        .await
                    {
                        Ok(r) => {
                            tracing::info!(set = %p.set, version, results = %r["results"], "published");
                            last = d;
                        }
                        Err(e) => tracing::warn!(set = %p.set, "publishing: {e}"),
                    }
                }
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
        SyncItem { name: name.into(), content: content.into() }
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
    fn a_set_reads_back_and_its_digest_follows_content() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("s")).unwrap();
        std::fs::write(d.path().join("s/SKILL.md"), "x").unwrap();
        std::fs::write(d.path().join(".hidden"), "y").unwrap();
        let a = read_set(d.path()).unwrap();
        assert_eq!(a.keys().collect::<Vec<_>>(), ["s/SKILL.md"]);
        std::fs::write(d.path().join("s/SKILL.md"), "z").unwrap();
        assert_ne!(digest(&a), digest(&read_set(d.path()).unwrap()));
    }
}
