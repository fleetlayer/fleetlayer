//! What a move carries, as one stream sealed to a key the destination made for this move (age X25519): a manifest
//! (every file's path under the home folder, mode, size and SHA-256, and the settings to write) followed by the
//! files' bytes in the manifest's order. The destination writes each file to a staging folder, checks its SHA-256
//! against the manifest, and moves nothing into place unless every file matches. Over SSH that's encrypted twice; the
//! seal is what keeps keys and tokens unreadable in any file the copy passes through.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::Digest;

const MAGIC: &[u8] = b"FLMOVE1\n";
/// No manifest is this big; a bigger length means a corrupt stream.
const MAX_MANIFEST: u64 = 64 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// Relative to the home folder: the same place under the destination's home.
    pub path: String,
    pub mode: u32,
    pub size: u64,
    pub sha256: String,
}

/// A settings change the destination makes once the files are in place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Setting {
    /// A whole settings file (control.yaml, gateway.yaml, node.yaml), with paths already rewritten for the
    /// destination's home. Never written over a different file.
    File { path: String, text: String },
    /// A plugin's block, added to the destination's node.yaml.
    Plugin { name: String, text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub component: String,
    pub files: Vec<FileEntry>,
    pub settings: Vec<Setting>,
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// The manifest entry for a file under `home`.
pub fn entry(home: &Path, path: &Path) -> anyhow::Result<FileEntry> {
    use std::os::unix::fs::PermissionsExt;
    let rel =
        path.strip_prefix(home).map_err(|_| anyhow::anyhow!("{} isn't under {}", path.display(), home.display()))?;
    let meta = std::fs::metadata(path)?;
    Ok(FileEntry {
        path: rel.display().to_string(),
        mode: meta.permissions().mode() & 0o7777,
        size: meta.len(),
        sha256: sha256_file(path)?,
    })
}

/// A relative path with no `..`, root or prefix: where a file may go under the destination's home.
pub fn safe_rel(rel: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(rel);
    anyhow::ensure!(
        !rel.is_empty() && p.components().all(|c| matches!(c, std::path::Component::Normal(_))),
        "{rel:?} isn't a plain path under the home folder"
    );
    Ok(p.to_path_buf())
}

/// Write the sealed stream: the manifest, then each file's bytes, read from under `home`.
pub fn write_sealed(recipient: &str, home: &Path, manifest: &Manifest, out: impl Write) -> anyhow::Result<()> {
    let r = age::x25519::Recipient::from_str(recipient).map_err(|e| anyhow::anyhow!("bad key {recipient}: {e}"))?;
    let enc = age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient))?;
    let mut w = enc.wrap_output(out)?;
    w.write_all(MAGIC)?;
    let m = serde_json::to_vec(manifest)?;
    w.write_all(&(m.len() as u64).to_be_bytes())?;
    w.write_all(&m)?;
    for f in &manifest.files {
        let path = home.join(safe_rel(&f.path)?);
        let mut file = std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let copied = std::io::copy(&mut (&mut file).take(f.size), &mut w)?;
        anyhow::ensure!(
            copied == f.size && file.read(&mut [0u8; 1])? == 0,
            "{} changed while it was being copied (is it still running?)",
            path.display()
        );
    }
    w.finish()?;
    Ok(())
}

/// Read a sealed stream into `staging` (a new, empty folder), checking every file's size and SHA-256. The manifest,
/// once every file matched.
pub fn read_sealed(identity: &age::x25519::Identity, input: impl Read, staging: &Path) -> anyhow::Result<Manifest> {
    use std::os::unix::fs::OpenOptionsExt;
    let dec = age::Decryptor::new(input)?;
    let mut r = dec.decrypt(std::iter::once(identity as &dyn age::Identity))?;
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    anyhow::ensure!(magic == MAGIC, "not a fleetlayer move stream");
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_be_bytes(len);
    anyhow::ensure!(len <= MAX_MANIFEST, "a corrupt move stream (manifest of {len} bytes)");
    let mut m = vec![0u8; len as usize];
    r.read_exact(&mut m)?;
    let manifest: Manifest = serde_json::from_slice(&m)?;
    for f in &manifest.files {
        let dest = staging.join(safe_rel(&f.path)?);
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&dest)?;
        let mut h = HashWriter { inner: &mut out, hash: sha2::Sha256::new() };
        let copied = std::io::copy(&mut (&mut r).take(f.size), &mut h)?;
        anyhow::ensure!(copied == f.size, "{}: the stream ended early", f.path);
        let got = hex::encode(h.hash.finalize());
        anyhow::ensure!(got == f.sha256, "{}: checksum {got} doesn't match the source's {}", f.path, f.sha256);
        out.sync_all()?;
    }
    anyhow::ensure!(r.read(&mut [0u8; 1])? == 0, "the move stream has more than its manifest lists");
    Ok(manifest)
}

struct HashWriter<'a, W: Write> {
    inner: &'a mut W,
    hash: sha2::Sha256,
}

impl<W: Write> Write for HashWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::secrecy::ExposeSecret;

    #[test]
    fn files_arrive_checked_and_sealed() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join(".config/fleetlayer")).unwrap();
        std::fs::write(src.path().join(".config/fleetlayer/node-key"), "AGE-SECRET-KEY-1SECRET\n").unwrap();
        std::fs::write(src.path().join("big"), vec![7u8; 300_000]).unwrap();
        let files = ["big", ".config/fleetlayer/node-key"]
            .iter()
            .map(|p| entry(src.path(), &src.path().join(p)).unwrap())
            .collect();
        let manifest = Manifest {
            component: "node".into(),
            files,
            settings: vec![Setting::File { path: ".config/fleetlayer/node.yaml".into(), text: "name: a\n".into() }],
        };
        let id = age::x25519::Identity::generate();
        let mut sealed = vec![];
        write_sealed(&id.to_public().to_string(), src.path(), &manifest, &mut sealed).unwrap();
        assert!(!sealed.windows(14).any(|w| w == b"AGE-SECRET-KEY"), "the key never travels in the clear");

        let staging = tempfile::tempdir().unwrap();
        let got = read_sealed(&id, sealed.as_slice(), staging.path()).unwrap();
        assert_eq!(got, manifest);
        assert_eq!(std::fs::read(staging.path().join("big")).unwrap(), vec![7u8; 300_000]);

        // Another key can't open it, and a flipped byte is caught.
        let other = tempfile::tempdir().unwrap();
        assert!(read_sealed(&age::x25519::Identity::generate(), sealed.as_slice(), other.path()).is_err());
        let mut bad = sealed.clone();
        let n = bad.len() - 100;
        bad[n] ^= 1;
        let other = tempfile::tempdir().unwrap();
        assert!(read_sealed(&id, bad.as_slice(), other.path()).is_err());
        let _ = id.to_string().expose_secret().len();
    }

    #[test]
    fn a_wrong_checksum_or_an_escaping_path_is_refused() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a"), "hello").unwrap();
        let mut e = entry(src.path(), &src.path().join("a")).unwrap();
        e.sha256 = "0".repeat(64);
        let m = Manifest { component: "x".into(), files: vec![e], settings: vec![] };
        let id = age::x25519::Identity::generate();
        let mut sealed = vec![];
        write_sealed(&id.to_public().to_string(), src.path(), &m, &mut sealed).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let err = read_sealed(&id, sealed.as_slice(), staging.path()).unwrap_err().to_string();
        assert!(err.contains("checksum"), "{err}");
        assert!(safe_rel("../etc/passwd").is_err());
        assert!(safe_rel("/etc/passwd").is_err());
        assert!(safe_rel(".config/fleetlayer/node.yaml").is_ok());
    }
}
