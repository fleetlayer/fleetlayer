//! Which node a secret bundle may come from. The node lets a plugin publish only bundles named after itself, but any
//! node can run a plugin of the same name: an agent's own node could publish `1password/<your node>` and swap in
//! values or wipe them. So a plugin applying secrets takes each bundle from one node only: the one its config names,
//! or else the first one that sent it (kept in a file, so it holds across restarts), and refuses any other.

use std::collections::BTreeMap;
use std::path::Path;

/// Whether `from` may send `bundle`: `configured` when the config names the source, else the node pinned in
/// `pin_file`, else `from` is pinned now. The error says what happened and what to do.
pub fn check(pin_file: &Path, bundle: &str, from: Option<&str>, configured: Option<&str>) -> Result<(), String> {
    let Some(from) = from else {
        return Err(format!(
            "{bundle} arrived without its sender's name (the node is older than this plugin): update fleetlayer-node"
        ));
    };
    if let Some(c) = configured {
        return if c == from {
            Ok(())
        } else {
            Err(format!(
                "{bundle} came from node {from}, and this plugin takes it only from {c} (its `from:`): refused"
            ))
        };
    }
    let mut pins: BTreeMap<String, String> =
        std::fs::read(pin_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    match pins.get(bundle) {
        Some(p) if p == from => Ok(()),
        Some(p) => Err(format!(
            "{bundle} came from node {from}, but it first came from {p}, so only {p} may send it: refused. If {from} \
             is its new source, set `from: {from}` in this plugin's config (or remove the entry from {})",
            pin_file.display()
        )),
        None => {
            pins.insert(bundle.to_string(), from.to_string());
            let text = serde_json::to_string_pretty(&pins).map_err(|e| e.to_string())?;
            crate::write::atomic(pin_file, &text).map_err(|e| format!("pinning {bundle}'s source: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_source_or_the_first_sender_is_the_only_one_accepted() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("state/sources.json");
        assert!(check(&pin, "cookies/x", Some("laptop"), Some("laptop")).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), Some("laptop")).unwrap_err();
        assert!(e.contains("only from laptop"), "{e}");
        // No configured source: the first sender is pinned, and holds.
        assert!(check(&pin, "cookies/x", Some("laptop"), None).is_ok());
        assert!(check(&pin, "cookies/x", Some("laptop"), None).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), None).unwrap_err();
        assert!(e.contains("first came from laptop") && e.contains("from: agents"), "{e}");
        // Each bundle has its own pin.
        assert!(check(&pin, "cookies/y", Some("agents"), None).is_ok());
        assert!(check(&pin, "cookies/x", None, None).unwrap_err().contains("update fleetlayer-node"));
    }
}
