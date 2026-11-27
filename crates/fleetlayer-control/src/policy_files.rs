//! Policy files the server applies itself (`serve --policy-file`), so a policy kept in git and mounted (a Kubernetes
//! ConfigMap) takes effect without a restart or a second writer. Each file is read at start and then every
//! [`EVERY`] by content (a ConfigMap update swaps a symlink, which file watchers miss). A changed, valid version 2
//! policy is stored as `acl apply` stores it, and decisions use it at once (`Authz::policy` reloads stored text). An
//! invalid one is refused: the stored policy stays, and `/healthz` reports the error until the file is fixed.
//!
//! Which tenant a file is for: `--policy-file <tenant slug>=<path>`; a bare `<path>` is the deployment's first tenant,
//! as with `acl apply`; a folder holds one `<tenant slug>.yaml` (or `.yml`) per tenant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::App;
use crate::db::Db;

pub const EVERY: Duration = Duration::from_secs(10);

/// One `--policy-file` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub tenant: Option<String>,
    pub path: PathBuf,
}

impl std::str::FromStr for Source {
    type Err = String;

    fn from_str(s: &str) -> Result<Source, String> {
        match s.split_once('=') {
            Some((t, p)) if !t.is_empty() && !t.contains('/') => {
                Ok(Source { tenant: Some(t.to_string()), path: PathBuf::from(p) })
            }
            _ if s.is_empty() => Err("an empty --policy-file".into()),
            _ => Ok(Source { tenant: None, path: PathBuf::from(s) }),
        }
    }
}

/// The files the sources name now, with their tenant slugs (None: the first tenant). A folder is listed afresh each
/// time, so a tenant's file added later is picked up.
pub fn files(sources: &[Source]) -> Vec<(Option<String>, PathBuf)> {
    let mut out = vec![];
    for s in sources {
        if s.path.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&s.path)
                .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            found.sort();
            for p in found {
                let yaml = p.extension().is_some_and(|e| e == "yaml" || e == "yml");
                // A ConfigMap's folder also holds its own `..data` links: only regular-looking names count.
                let name = p.file_stem().and_then(|n| n.to_str()).filter(|n| !n.starts_with('.'));
                if let (true, Some(n)) = (yaml, name) {
                    out.push((Some(n.to_string()), p.clone()));
                }
            }
        } else {
            out.push((s.tenant.clone(), s.path.clone()));
        }
    }
    out
}

/// What applying a version 2 policy text to a tenant changed: the removed and added lines, or None when it already
/// matches. With `check`, nothing is stored.
pub async fn apply_text(db: &Db, tenant: &str, text: &str, check: bool) -> anyhow::Result<Option<Vec<String>>> {
    crate::policy::Compiled::parse(text)?;
    let old = crate::authz::stored(db, tenant).await?.unwrap_or_default();
    if old == text {
        return Ok(None);
    }
    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (old.lines().collect(), text.lines().collect());
    let mut diff: Vec<String> = old_lines.iter().filter(|l| !new_lines.contains(l)).map(|l| format!("- {l}")).collect();
    diff.extend(new_lines.iter().filter(|l| !old_lines.contains(l)).map(|l| format!("+ {l}")));
    if !check {
        crate::authz::store(db, tenant, text).await?;
    }
    Ok(Some(diff))
}

/// Apply one file: Ok(true) when the stored policy changed.
async fn apply_file(db: &Db, tenant: Option<&str>, path: &Path) -> anyhow::Result<bool> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading it: {e}"))?;
    let v: serde_json::Value = yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("not YAML: {e}"))?;
    anyhow::ensure!(
        v["version"].as_i64() == Some(2),
        "not a version 2 policy (version: 2); apply version 1 rules with acl apply"
    );
    let t = crate::people::tenant_or_first(db, tenant).await?;
    match apply_text(db, &t.id, &text, false).await? {
        None => Ok(false),
        Some(diff) => {
            tracing::info!(file = %path.display(), tenant = %t.slug, "access policy applied from its file:\n{}", diff.join("\n"));
            Ok(true)
        }
    }
}

/// Apply every file once, recording each one's error (or clearing it) for `/healthz`.
pub async fn apply_all(app: &App, sources: &[Source]) {
    let mut errors = BTreeMap::new();
    for (tenant, path) in files(sources) {
        if let Err(e) = apply_file(&app.db, tenant.as_deref(), &path).await {
            let msg = format!("the policy file {} isn't applied: {e}", path.display());
            errors.insert(path.display().to_string(), msg);
        }
    }
    let mut cur = app.authz.file_errors.lock().unwrap_or_else(|e| e.into_inner());
    for (path, msg) in &errors {
        if cur.get(path) != Some(msg) {
            tracing::error!("{msg}; the stored policy stays");
        }
    }
    for path in cur.keys().filter(|p| !errors.contains_key(*p)) {
        tracing::info!("the policy file {path} applies again");
    }
    *cur = errors;
}

/// Apply the files at start, then every [`EVERY`].
pub async fn run(app: Arc<App>, sources: Vec<Source>) {
    loop {
        apply_all(&app, &sources).await;
        tokio::time::sleep(EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app() -> Arc<App> {
        Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        })
    }

    const DENY_TRIAGE: &str = "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"triage@*\", can: message }\n";
    const ALLOW_ALL: &str = "version: 2\ndefault: allow\nask: []\n";

    async fn allowed(app: &App, from: &str) -> bool {
        crate::authz::check(app, "default", from, "message", "b@laptop").await.is_ok()
    }

    #[test]
    fn sources_name_their_tenant() {
        assert_eq!(
            "acme=/p/acme.yaml".parse(),
            Ok(Source { tenant: Some("acme".into()), path: "/p/acme.yaml".into() })
        );
        assert_eq!("/p/policy.yaml".parse(), Ok(Source { tenant: None, path: "/p/policy.yaml".into() }));
        assert_eq!("./a=b.yaml".parse(), Ok(Source { tenant: None, path: "./a=b.yaml".into() }));
    }

    #[tokio::test]
    async fn a_changed_file_applies_without_a_restart_and_a_broken_one_keeps_the_old_policy() {
        let app = app().await;
        crate::db::enrol(&app.db, "default", "laptop").await.unwrap();
        crate::people::agent_admit(&app.db, "default", "*@laptop", "admin").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("policy.yaml");
        let sources = vec![Source { tenant: None, path: file.clone() }];

        std::fs::write(&file, DENY_TRIAGE).unwrap();
        apply_all(&app, &sources).await;
        assert_eq!(crate::authz::stored(&app.db, "default").await.unwrap().as_deref(), Some(DENY_TRIAGE));
        assert!(!allowed(&app, "triage@laptop").await);
        assert!(allowed(&app, "planning@laptop").await);

        std::fs::write(&file, ALLOW_ALL).unwrap();
        apply_all(&app, &sources).await;
        assert!(allowed(&app, "triage@laptop").await, "the running server decides with the new file");

        std::fs::write(&file, "version: 2\ndefault: sometimes\n").unwrap();
        apply_all(&app, &sources).await;
        assert_eq!(crate::authz::stored(&app.db, "default").await.unwrap().as_deref(), Some(ALLOW_ALL), "kept");
        let errors = app.authz.file_errors.lock().unwrap().clone();
        assert!(errors.values().next().unwrap().contains("isn't applied"), "{errors:?}");

        std::fs::write(&file, DENY_TRIAGE).unwrap();
        apply_all(&app, &sources).await;
        assert!(app.authz.file_errors.lock().unwrap().is_empty(), "fixed: no longer degraded");
        assert!(!allowed(&app, "triage@laptop").await);

        std::fs::write(&file, "default: allow\nrules: []\n").unwrap();
        apply_all(&app, &sources).await;
        let errors = app.authz.file_errors.lock().unwrap().clone();
        assert!(errors.values().next().unwrap().contains("not a version 2 policy"), "{errors:?}");
    }

    #[tokio::test]
    async fn a_folder_holds_one_file_per_tenant() {
        let app = app().await;
        let acme = crate::people::tenant_add(&app.db, "acme", "Acme", "admin").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("acme.yaml"), DENY_TRIAGE).unwrap();
        std::fs::write(dir.path().join("nobody.yaml"), ALLOW_ALL).unwrap();
        std::fs::write(dir.path().join("README.md"), "not a policy").unwrap();
        let sources = vec![Source { tenant: None, path: dir.path().to_path_buf() }];
        apply_all(&app, &sources).await;
        assert_eq!(crate::authz::stored(&app.db, &acme.id).await.unwrap().as_deref(), Some(DENY_TRIAGE));
        let errors = app.authz.file_errors.lock().unwrap().clone();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors.values().next().unwrap().contains("no tenant called nobody"), "{errors:?}");
    }
}
