//! The node's own key: an age X25519 identity, made on first run and kept in `~/.config/fleetlayer/node-key` (0600).
//! Its public half goes to the control plane in `link/hello`; other nodes seal secrets to it (`docs/protocol.md`
//! section 8), and only this node can open them. The private half never leaves this file.

use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use age::x25519::Identity;

/// Load the node's identity, or make one and save it.
pub fn load_or_create(path: &Path) -> anyhow::Result<Identity> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let line = text.lines().find(|l| l.starts_with("AGE-SECRET-KEY-")).unwrap_or_default();
            Identity::from_str(line.trim()).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::OpenOptionsExt;
            let id = Identity::generate();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            use age::secrecy::ExposeSecret;
            writeln!(f, "# fleetlayer node key (age X25519). Public: {}", id.to_public())?;
            writeln!(f, "{}", id.to_string().expose_secret())?;
            tracing::info!(public = %id.to_public(), "made this node's key");
            Ok(id)
        }
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// A short fingerprint for showing a public key to a person: the first 16 hex characters of its SHA-256.
pub fn fingerprint(public: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(public.as_bytes()))[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_made_once_and_read_back() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("node-key");
        let a = load_or_create(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.to_public().to_string(), b.to_public().to_string());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(a.to_public().to_string().starts_with("age1"));
        assert_eq!(fingerprint("age1abc").len(), 16);
    }
}
