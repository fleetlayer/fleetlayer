//! A bundle's own signature, so a target can trust its source by a key that moves with it rather than by the node it
//! runs on. A source keeps an Ed25519 key in its own state (the Claude hub's folder, which `fleetlayer move` carries),
//! adds an item [`SIGNATURE_ITEM`] to each bundle it publishes, and a target pins that key ([`crate::pin`]): the hub
//! can move to another machine, under another node name, and its targets keep taking its bundles with nothing to
//! change. Any other node publishing a bundle of the same name can't sign it with that key, and is refused.
//!
//! The signature covers the bundle's name, its version and every other item (name, content, mode, link), so a signed
//! bundle can't be replayed under a newer version or with an item changed.

use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The item that carries a bundle's signature: `{"key": "ed25519:<base64>", "sig": "<base64>"}`.
pub const SIGNATURE_ITEM: &str = ".fleetlayer-signature";

/// What a key is pinned as: `ed25519:<base64 of the public key>`.
pub const KEY_PREFIX: &str = "ed25519:";

/// One item of a bundle, as the signature sees it.
#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    pub name: &'a str,
    pub content: &'a str,
    pub mode: Option<u32>,
    pub link: Option<&'a str>,
}

/// A source's signing key.
pub struct BundleKey(SigningKey);

impl BundleKey {
    /// The key kept at `path` (its 32-byte seed in base64, mode 0600), made there only when there's no file at all.
    /// A file that's there but can't be read or isn't a key is an error, never replaced: a new key would make every
    /// target refuse this source.
    pub fn load_or_create(path: &Path) -> Result<BundleKey, String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let b64 = base64::engine::general_purpose::STANDARD;
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let seed: [u8; 32] = b64
                    .decode(text.trim())
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| format!("{} isn't a bundle signing key", path.display()))?;
                return Ok(BundleKey(SigningKey::from_bytes(&seed)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("reading the bundle signing key {}: {e}", path.display())),
        }
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("making a bundle signing key: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        // create_new: if another process made one meanwhile, its key stands and this one isn't written.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("keeping the bundle signing key at {}: {e}", path.display()))?;
        f.write_all(b64.encode(seed).as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("keeping the bundle signing key at {}: {e}", path.display()))?;
        Ok(BundleKey(SigningKey::from_bytes(&seed)))
    }

    /// The public key, as targets pin it (`ed25519:<base64>`).
    pub fn id(&self) -> String {
        key_id(&self.0.verifying_key())
    }

    /// The signature item's content for `bundle` at `version` with these items.
    pub fn sign(&self, bundle: &str, version: u64, items: &[Part]) -> String {
        let sig = self.0.sign(&message(bundle, version, items));
        serde_json::json!({"key": self.id(), "sig": base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())})
            .to_string()
    }
}

fn key_id(k: &VerifyingKey) -> String {
    format!("{KEY_PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(k.to_bytes()))
}

/// The signed bytes: a fixed header, the bundle's name and version, then every item but the signature, by name.
fn message(bundle: &str, version: u64, items: &[Part]) -> Vec<u8> {
    let mut parts: Vec<(&str, &str, Option<u32>, Option<&str>)> =
        items.iter().filter(|p| p.name != SIGNATURE_ITEM).map(|p| (p.name, p.content, p.mode, p.link)).collect();
    parts.sort();
    // Arrays and strings only: their JSON is the same whatever serde_json's features.
    let body = serde_json::to_string(&(bundle, version, parts)).unwrap_or_default();
    format!("fleetlayer bundle signature v1\n{body}").into_bytes()
}

/// The key that signed a bundle: None when it carries no signature, an error when it carries one that doesn't hold.
pub fn verify(bundle: &str, version: u64, items: &[Part]) -> Result<Option<String>, String> {
    // Names are unique, the signature's included: two items of one name (or two signatures) could be read one way here
    // and another way by the plugin that applies them.
    let mut names = std::collections::BTreeSet::new();
    if let Some(dup) = items.iter().find(|p| !names.insert(p.name)) {
        return Err(format!("{bundle} has two items called {:?}: refused", dup.name));
    }
    let Some(item) = items.iter().find(|p| p.name == SIGNATURE_ITEM) else { return Ok(None) };
    let b64 = base64::engine::general_purpose::STANDARD;
    let v: serde_json::Value =
        serde_json::from_str(item.content).map_err(|_| format!("{bundle}'s signature isn't readable"))?;
    let key = v["key"].as_str().unwrap_or_default();
    let raw: [u8; 32] = key
        .strip_prefix(KEY_PREFIX)
        .and_then(|k| b64.decode(k).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("{bundle}'s signature names no Ed25519 key"))?;
    let vk =
        VerifyingKey::from_bytes(&raw).map_err(|_| format!("{bundle}'s signature names a key that isn't valid"))?;
    let sig: [u8; 64] = v["sig"]
        .as_str()
        .and_then(|s| b64.decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("{bundle}'s signature isn't readable"))?;
    vk.verify_strict(&message(bundle, version, items), &Signature::from_bytes(&sig))
        .map_err(|_| format!("{bundle}'s signature doesn't match its contents: refused"))?;
    Ok(Some(key_id(&vk)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part<'a>(name: &'a str, content: &'a str) -> Part<'a> {
        Part { name, content, mode: None, link: None }
    }

    #[test]
    fn a_signed_bundle_verifies_and_any_change_breaks_it() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("hub/signing-key");
        let key = BundleKey::load_or_create(&path).unwrap();
        assert_eq!(BundleKey::load_or_create(&path).unwrap().id(), key.id(), "kept, not made again");
        let items = [part("accounts.json", "[1]")];
        let sig = key.sign("claude-tokens", 7, &items);
        let signed = [part("accounts.json", "[1]"), part(SIGNATURE_ITEM, &sig)];
        assert_eq!(verify("claude-tokens", 7, &signed), Ok(Some(key.id())));
        assert!(verify("claude-tokens", 8, &signed).is_err(), "a newer version");
        assert!(verify("other", 7, &signed).is_err(), "another bundle");
        let changed = [part("accounts.json", "[2]"), part(SIGNATURE_ITEM, &sig)];
        assert!(verify("claude-tokens", 7, &changed).is_err(), "a changed item");
        let added = [part("accounts.json", "[1]"), part("extra", "x"), part(SIGNATURE_ITEM, &sig)];
        assert!(verify("claude-tokens", 7, &added).is_err(), "an added item");
        assert_eq!(verify("claude-tokens", 7, &items), Ok(None), "unsigned");
        let other = BundleKey::load_or_create(&d.path().join("other")).unwrap();
        assert_ne!(other.id(), key.id());
        let twice = [part("accounts.json", "[1]"), part(SIGNATURE_ITEM, &sig), part(SIGNATURE_ITEM, &sig)];
        assert!(verify("claude-tokens", 7, &twice).unwrap_err().contains("two items"));
        let dup = [part("accounts.json", "[1]"), part("accounts.json", "[2]")];
        assert!(verify("claude-tokens", 7, &dup).is_err(), "duplicate names, signed or not");
    }

    #[test]
    fn an_unreadable_key_is_an_error_never_replaced() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("signing-key");
        std::fs::write(&path, "not a key").unwrap();
        assert!(BundleKey::load_or_create(&path).err().unwrap().contains("isn't a bundle signing key"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key", "left as it was");
        let dir = d.path().join("a-directory");
        std::fs::create_dir(&dir).unwrap();
        assert!(BundleKey::load_or_create(&dir).is_err());
        let made = d.path().join("new/signing-key");
        BundleKey::load_or_create(&made).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&made).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
