//! Which source a secret bundle may come from. The node lets a plugin publish only bundles named after itself, but any
//! node can run a plugin of the same name: an agent's own node could publish `1password/<your node>` and swap in
//! values or wipe them. So a plugin applying secrets takes each bundle from one source only: the node its config names,
//! or else the first one that sent it (kept in a file, so it holds across restarts), and refuses any other.
//!
//! A source that signs its bundles ([`crate::sign`]) is pinned by its key instead of its node: the first signed bundle
//! pins the key (a bundle pinned to a node moves to the key when that node sends it signed), and from then on only
//! bundles signed by that key are taken, from whichever node, so the source can move to another machine. An unsigned
//! bundle is refused once a key is pinned.

use std::collections::BTreeMap;
use std::path::Path;

use crate::sign::KEY_PREFIX;

/// Whether `from` may send `bundle`, signed by `signer` (the key [`crate::sign::verify`] found, if any): `configured`
/// when the config names the source node, else the node or key pinned in `pin_file`, else this one is pinned now. The
/// error says what happened and what to do.
pub fn check(
    pin_file: &Path,
    bundle: &str,
    from: Option<&str>,
    configured: Option<&str>,
    signer: Option<&str>,
) -> Result<(), String> {
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
    let save = |pins: &BTreeMap<String, String>| {
        let text = serde_json::to_string_pretty(pins).map_err(|e| e.to_string())?;
        crate::write::atomic(pin_file, &text).map_err(|e| format!("pinning {bundle}'s source: {e}"))
    };
    match pins.get(bundle).cloned() {
        Some(p) if p.starts_with(KEY_PREFIX) => match signer {
            Some(k) if k == p => Ok(()),
            Some(_) => Err(format!(
                "{bundle} came from node {from} signed by another key than its source's: refused. If its source \
                 really has a new key, remove the entry from {}",
                pin_file.display()
            )),
            None => Err(format!(
                "{bundle} came from node {from} unsigned, but its source signs it: refused (an older source, or \
                 another node sending a bundle of the same name)"
            )),
        },
        Some(p) if p == from => {
            if let Some(k) = signer {
                // The pinned node now signs: trust its key from here on, wherever it moves.
                pins.insert(bundle.to_string(), k.to_string());
                save(&pins)?;
            }
            Ok(())
        }
        Some(p) => Err(format!(
            "{bundle} came from node {from}, but it first came from {p}, so only {p} may send it: refused. If {from} \
             is its new source, set `from: {from}` in this plugin's config (or remove the entry from {})",
            pin_file.display()
        )),
        None => {
            pins.insert(bundle.to_string(), signer.unwrap_or(from).to_string());
            save(&pins)
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
        assert!(check(&pin, "cookies/x", Some("laptop"), Some("laptop"), None).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), Some("laptop"), None).unwrap_err();
        assert!(e.contains("only from laptop"), "{e}");
        // No configured source: the first sender is pinned, and holds.
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None).is_ok());
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), None, None).unwrap_err();
        assert!(e.contains("first came from laptop") && e.contains("from: agents"), "{e}");
        // Each bundle has its own pin.
        assert!(check(&pin, "cookies/y", Some("agents"), None, None).is_ok());
        assert!(check(&pin, "cookies/x", None, None, None).unwrap_err().contains("update fleetlayer-node"));
    }

    #[test]
    fn a_signing_source_is_pinned_by_its_key_and_may_move() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("sources.json");
        let (k, other) = ("ed25519:AAAA", "ed25519:BBBB");
        // Pinned to its node by an older release, then the same node signs: the pin moves to the key.
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, None).is_ok());
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, Some(k)).is_ok());
        assert!(std::fs::read_to_string(&pin).unwrap().contains(k));
        // The hub moved: another node, the same key.
        assert!(check(&pin, "claude-tokens", Some("server-claudehub"), None, Some(k)).is_ok());
        // Another key, or no signature, from any node: refused.
        assert!(check(&pin, "claude-tokens", Some("agents"), None, Some(other)).unwrap_err().contains("another key"));
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, None).unwrap_err().contains("unsigned"));
        // A first delivery that's signed pins the key straight away.
        assert!(check(&pin, "fresh", Some("x"), None, Some(k)).is_ok());
        assert!(check(&pin, "fresh", Some("y"), None, Some(k)).is_ok());
        // A node pin and another node's signed bundle: still refused (it isn't the pinned source).
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None).is_ok());
        assert!(
            check(&pin, "cookies/x", Some("agents"), None, Some(k)).unwrap_err().contains("first came from laptop")
        );
    }
}
