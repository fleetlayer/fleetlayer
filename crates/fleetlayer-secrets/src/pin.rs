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

use serde_json::{Value, json};

use crate::sign::KEY_PREFIX;

/// A pin that changed with this delivery: the bundle's trusted source was `was` (None: it had none, and a signing key
/// is pinned on first sight) and is now `now`. Never silent: the plugin logs it and answers it to the node, which
/// publishes it as a `sync.pin` reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinChange {
    pub bundle: String,
    pub was: Option<String>,
    pub now: String,
}

impl PinChange {
    /// What a plugin adds to its `sync/apply` answer (`pinChanged`).
    pub fn to_json(&self) -> Value {
        json!({"bundle": self.bundle, "was": self.was, "now": self.now})
    }
}

/// One bundle's entry: its trusted source (a node, or `ed25519:<key>`) and the newest version taken from it. An entry
/// written before versions were kept is the source alone.
#[derive(Debug, Clone, Default)]
struct Entry {
    source: Option<String>,
    version: u64,
}

fn read(pin_file: &Path) -> BTreeMap<String, Entry> {
    let raw: BTreeMap<String, Value> =
        std::fs::read(pin_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    raw.into_iter()
        .map(|(k, v)| {
            let e = match v {
                Value::String(s) => Entry { source: Some(s), version: 0 },
                v => Entry {
                    source: v["source"].as_str().map(str::to_string),
                    version: v["version"].as_u64().unwrap_or(0),
                },
            };
            (k, e)
        })
        .collect()
}

fn write(pin_file: &Path, pins: &BTreeMap<String, Entry>) -> Result<(), String> {
    let raw: BTreeMap<&String, Value> =
        pins.iter().map(|(k, e)| (k, json!({"source": e.source, "version": e.version}))).collect();
    let text = serde_json::to_string_pretty(&raw).map_err(|e| e.to_string())?;
    crate::write::atomic(pin_file, &text)
        .map_err(|e| format!("keeping the bundles' sources in {}: {e}", pin_file.display()))
}

/// Whether `from` may send `version` of `bundle`, signed by `signer` (the key [`crate::sign::verify`] found, if any):
/// `configured` when the config names the source node, else the node or key pinned in `pin_file`, else this one is
/// pinned now. A version older than the newest already taken is refused whatever its source (a stale bundle replayed
/// to a target that forgot what it had), and that high-water mark outlives `sync/forget`. The error says what
/// happened and what to do; `Ok(Some(change))` when the trusted source changed.
pub fn check(
    pin_file: &Path,
    bundle: &str,
    from: Option<&str>,
    configured: Option<&str>,
    signer: Option<&str>,
    version: u64,
) -> Result<Option<PinChange>, String> {
    let Some(from) = from else {
        return Err(format!(
            "{bundle} arrived without its sender's name (the node is older than this plugin): update fleetlayer-node"
        ));
    };
    let mut pins = read(pin_file);
    let entry = pins.get(bundle).cloned().unwrap_or_default();
    if version < entry.version {
        return Err(format!(
            "{bundle} version {version} is older than version {} already taken from its source: refused (a stale \
             copy sent again)",
            entry.version
        ));
    }
    let mut change = None;
    let source = if let Some(c) = configured {
        if c != from {
            return Err(format!(
                "{bundle} came from node {from}, and this plugin takes it only from {c} (its `from:`): refused"
            ));
        }
        entry.source.clone()
    } else {
        match entry.source.clone() {
            Some(p) if p.starts_with(KEY_PREFIX) => match signer {
                Some(k) if k == p => Some(p),
                Some(_) => {
                    return Err(format!(
                        "{bundle} came from node {from} signed by another key than its source's: refused. If its \
                         source really has a new key, run `fleetlayer secrets unpin {bundle}` on this machine"
                    ));
                }
                None => {
                    return Err(format!(
                        "{bundle} came from node {from} unsigned, but its source signs it: refused (an older source, \
                         or another node sending a bundle of the same name)"
                    ));
                }
            },
            Some(p) if p == from => match signer {
                // The pinned node now signs: trust its key from here on, wherever it moves.
                Some(k) => {
                    change = Some(PinChange { bundle: bundle.into(), was: Some(p), now: k.into() });
                    Some(k.to_string())
                }
                None => Some(p),
            },
            Some(p) => {
                return Err(format!(
                    "{bundle} came from node {from}, but it first came from {p}, so only {p} may send it: refused. If \
                     {from} is its new source, set `from: {from}` in this plugin's config, or run `fleetlayer secrets \
                     unpin {bundle}` on this machine"
                ));
            }
            // A first pin to a key is reported too: from here on, only that key's bundles are taken.
            None => match signer {
                Some(k) => {
                    change = Some(PinChange { bundle: bundle.into(), was: None, now: k.into() });
                    Some(k.to_string())
                }
                None => Some(from.to_string()),
            },
        }
    };
    let updated = Entry { source, version: version.max(entry.version) };
    if pins.get(bundle).is_none_or(|e| e.source != updated.source || e.version != updated.version) {
        pins.insert(bundle.to_string(), updated);
        write(pin_file, &pins)?;
    }
    Ok(change)
}

/// Forget `bundle`'s trusted source in `pin_file`, so the next sender is trusted from scratch (`fleetlayer secrets
/// unpin`). Its newest version is kept, so a stale copy is still refused. Answers what it was pinned to.
pub fn unpin(pin_file: &Path, bundle: &str) -> Result<Option<String>, String> {
    let mut pins = read(pin_file);
    let Some(e) = pins.get_mut(bundle) else { return Ok(None) };
    let Some(was) = e.source.take() else { return Ok(None) };
    write(pin_file, &pins)?;
    Ok(Some(was))
}

/// The bundles pinned in `pin_file`, with their sources.
pub fn list(pin_file: &Path) -> Vec<(String, String)> {
    read(pin_file).into_iter().filter_map(|(b, e)| Some((b, e.source?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_source_or_the_first_sender_is_the_only_one_accepted() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("state/sources.json");
        assert!(check(&pin, "cookies/x", Some("laptop"), Some("laptop"), None, 1).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), Some("laptop"), None, 1).unwrap_err();
        assert!(e.contains("only from laptop"), "{e}");
        // No configured source: the first sender is pinned, and holds.
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None, 1).is_ok());
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None, 1).is_ok());
        let e = check(&pin, "cookies/x", Some("agents"), None, None, 1).unwrap_err();
        assert!(e.contains("first came from laptop") && e.contains("from: agents"), "{e}");
        // Each bundle has its own pin.
        assert!(check(&pin, "cookies/y", Some("agents"), None, None, 1).is_ok());
        assert!(check(&pin, "cookies/x", None, None, None, 1).unwrap_err().contains("update fleetlayer-node"));
    }

    #[test]
    fn a_signing_source_is_pinned_by_its_key_and_may_move() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("sources.json");
        let (k, other) = ("ed25519:AAAA", "ed25519:BBBB");
        // Pinned to its node by an older release, then the same node signs: the pin moves to the key.
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, None, 1).is_ok());
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, Some(k), 1).is_ok());
        assert!(std::fs::read_to_string(&pin).unwrap().contains(k));
        // The hub moved: another node, the same key.
        assert!(check(&pin, "claude-tokens", Some("server-claudehub"), None, Some(k), 1).is_ok());
        // Another key, or no signature, from any node: refused.
        assert!(
            check(&pin, "claude-tokens", Some("agents"), None, Some(other), 1).unwrap_err().contains("another key")
        );
        assert!(check(&pin, "claude-tokens", Some("laptop"), None, None, 1).unwrap_err().contains("unsigned"));
        // A first delivery that's signed pins the key straight away, and says so.
        let first = check(&pin, "fresh", Some("x"), None, Some(k), 1).unwrap().unwrap();
        assert_eq!((first.was, first.now.as_str()), (None, k));
        assert_eq!(
            check(&pin, "cookies/new", Some("x"), None, None, 1),
            Ok(None),
            "a first node pin isn't a key change"
        );
        assert!(check(&pin, "fresh", Some("y"), None, Some(k), 1).is_ok());
        // A node pin and another node's signed bundle: still refused (it isn't the pinned source).
        assert!(check(&pin, "cookies/x", Some("laptop"), None, None, 1).is_ok());
        assert!(
            check(&pin, "cookies/x", Some("agents"), None, Some(k), 1).unwrap_err().contains("first came from laptop")
        );
    }

    #[test]
    fn a_node_pin_moving_to_its_key_is_reported_and_unpin_starts_afresh() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("sources.json");
        // A pin file from before versions were kept.
        std::fs::write(&pin, r#"{"claude-tokens": "laptop"}"#).unwrap();
        assert_eq!(check(&pin, "claude-tokens", Some("laptop"), None, None, 5), Ok(None));
        let change = check(&pin, "claude-tokens", Some("laptop"), None, Some("ed25519:K"), 6).unwrap().unwrap();
        assert_eq!((change.was.as_deref(), change.now.as_str()), (Some("laptop"), "ed25519:K"));
        assert_eq!(
            check(&pin, "claude-tokens", Some("server"), None, Some("ed25519:K"), 7),
            Ok(None),
            "no change, no report"
        );
        assert_eq!(list(&pin), [("claude-tokens".to_string(), "ed25519:K".to_string())]);
        assert_eq!(unpin(&pin, "claude-tokens").unwrap().as_deref(), Some("ed25519:K"));
        assert_eq!(unpin(&pin, "claude-tokens").unwrap(), None);
        assert!(check(&pin, "claude-tokens", Some("new"), None, Some("ed25519:N"), 8).is_ok(), "trusted afresh");
    }

    #[test]
    fn an_older_version_is_refused_even_after_forgetting() {
        let d = tempfile::tempdir().unwrap();
        let pin = d.path().join("sources.json");
        assert!(check(&pin, "b", Some("hub"), None, None, 10).is_ok());
        assert!(check(&pin, "b", Some("hub"), None, None, 10).is_ok(), "the same version again (after a forget)");
        assert!(check(&pin, "b", Some("hub"), None, None, 9).unwrap_err().contains("older than version 10"));
        assert!(check(&pin, "b", Some("hub"), Some("hub"), None, 9).is_err(), "with from: too");
        unpin(&pin, "b").unwrap();
        assert!(check(&pin, "b", Some("other"), None, None, 9).is_err(), "unpinning keeps the high-water mark");
    }
}
