//! Everything the sign-in service keeps, in one folder (`dir`, made 0700): `people.json` (each person's passkeys),
//! `keys/signing.pem` (the token-signing key) and `sessions.json` (who is signed in on which browser, by the SHA-256
//! of the cookie). Copy the folder and the settings file to move the service; nothing else holds state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use webauthn_rs::prelude::{Passkey, Uuid};

/// One passkey, with what the person sees about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPasskey {
    pub passkey: Passkey,
    /// The WebAuthn user handle this passkey carries (a recovery adds one with a new handle to the same person).
    pub user_id: Uuid,
    pub added_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

/// A person's credentials. Who they are and where they belong is the control plane's; this is only how they prove it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// The user handle for passkeys they add while signed in.
    pub user_id: Uuid,
    #[serde(default)]
    pub passkeys: Vec<StoredPasskey>,
    /// Accounts at upstream providers that sign them in (their issuer and subject, never an email).
    #[serde(default)]
    pub links: Vec<Link>,
}

/// An upstream identity linked to a person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub upstream: String,
    pub iss: String,
    pub sub: String,
    pub added_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct People {
    /// By person ID (`p_…`).
    #[serde(default)]
    pub people: BTreeMap<String, Credentials>,
}

/// A browser's sign-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub person: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// When they last proved it with a passkey: changes that can't be undone easily ask again after ten minutes.
    #[serde(default)]
    pub verified_at: i64,
    /// Signed in through an upstream provider: the one tenant it signs in for (its ID).
    #[serde(default)]
    pub via_tenant: Option<String>,
    /// The services it signed in to, which are sent a logout when it ends.
    #[serde(default)]
    pub clients: BTreeSet<String>,
    /// Forward-auth cookies it set (their SHA-256): they work while it lasts.
    #[serde(default)]
    pub forward: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sessions {
    /// By the SHA-256 of the cookie's value, so the file holds no cookie.
    #[serde(default)]
    pub sessions: BTreeMap<String, Session>,
}

pub struct Store {
    pub dir: PathBuf,
    pub people: Mutex<People>,
    pub sessions: Mutex<Sessions>,
    /// Sign-outs still to deliver to services (`logouts.json`).
    pub logouts: Mutex<crate::logout::Outbox>,
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Write a file whole or not at all (a temporary file, then a rename), readable by this user only.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

impl Store {
    pub fn open(dir: &Path) -> anyhow::Result<Store> {
        std::fs::create_dir_all(dir.join("keys"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let people = read_json(&dir.join("people.json"))?;
        let sessions = read_json(&dir.join("sessions.json"))?;
        let logouts = read_json(&dir.join("logouts.json"))?;
        Ok(Store {
            dir: dir.to_path_buf(),
            people: Mutex::new(people),
            sessions: Mutex::new(sessions),
            logouts: Mutex::new(logouts),
        })
    }

    pub fn save_people(&self, people: &People) -> anyhow::Result<()> {
        write_private(&self.dir.join("people.json"), &serde_json::to_vec_pretty(people)?)
    }

    pub fn save_sessions(&self, sessions: &Sessions) -> anyhow::Result<()> {
        write_private(&self.dir.join("sessions.json"), &serde_json::to_vec_pretty(sessions)?)
    }

    pub fn save_logouts(&self, out: &crate::logout::Outbox) -> anyhow::Result<()> {
        write_private(&self.dir.join("logouts.json"), &serde_json::to_vec_pretty(out)?)
    }

    /// Whether the folder can still be written (for `/healthz`).
    pub fn writable(&self) -> bool {
        let probe = self.dir.join(".probe");
        let ok = std::fs::write(&probe, b"ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }
}
