//! Reading cookies from a Chromium-family browser's cookie database (Chrome, Chromium, Brave, Edge), written from the
//! format's public descriptions: an SQLite file with a `cookies` table, values encrypted with AES-128-CBC under a key
//! derived with PBKDF2-HMAC-SHA1 (salt `saltysalt`, a 16-byte IV of spaces):
//!
//! - **macOS**, prefix `v10`: the password is the browser's "Safe Storage" item in the login Keychain, 1003 iterations.
//! - **Linux**, prefix `v10`: the fixed password `peanuts`, one iteration. Prefix `v11`: the password the browser keeps
//!   in the desktop keyring (libsecret, read with `secret-tool`), one iteration.
//! - Since the database's `meta` version 24, the decrypted value starts with the SHA-256 of the cookie's `host_key`,
//!   which is checked and removed.
//!
//! Windows (DPAPI, and app-bound `v20` values since Chrome 127) isn't supported.

use std::path::{Path, PathBuf};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One cookie, as the bundle carries it. `expires` is Unix seconds, None for a session cookie.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    /// The browser's `host_key`: `.x.com` (this domain and its subdomains) or `x.com` (this host only).
    pub domain: String,
    pub name: String,
    pub value: String,
    pub path: String,
    #[serde(default)]
    pub expires: Option<i64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// `Strict`, `Lax` or `None`; None when the browser left it unspecified.
    #[serde(default)]
    pub same_site: Option<String>,
}

impl Cookie {
    /// For this host only (no leading dot).
    pub fn host_only(&self) -> bool {
        !self.domain.starts_with('.')
    }
}

/// Whether the cookie's `host_key` belongs to one of `domains` (each a plain host name such as `x.com`): that host or
/// one of its subdomains.
pub fn in_domains(host_key: &str, domains: &[String]) -> bool {
    let host = host_key.trim_start_matches('.').to_ascii_lowercase();
    domains.iter().any(|d| {
        let d = d.trim_start_matches('.').to_ascii_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Browser {
    #[default]
    Chrome,
    Chromium,
    Brave,
    Edge,
}

impl Browser {
    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Chromium => "Chromium",
            Browser::Brave => "Brave",
            Browser::Edge => "Edge",
        }
    }

    /// The browser's user data folder under `home`, on this system.
    pub fn user_data_dir(self, home: &Path) -> PathBuf {
        if cfg!(target_os = "macos") {
            let base = home.join("Library/Application Support");
            match self {
                Browser::Chrome => base.join("Google/Chrome"),
                Browser::Chromium => base.join("Chromium"),
                Browser::Brave => base.join("BraveSoftware/Brave-Browser"),
                Browser::Edge => base.join("Microsoft Edge"),
            }
        } else {
            let base = home.join(".config");
            match self {
                Browser::Chrome => base.join("google-chrome"),
                Browser::Chromium => base.join("chromium"),
                Browser::Brave => base.join("BraveSoftware/Brave-Browser"),
                Browser::Edge => base.join("microsoft-edge"),
            }
        }
    }

    /// The Keychain item (service, account) holding the macOS password.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn keychain_item(self) -> (&'static str, &'static str) {
        match self {
            Browser::Chrome => ("Chrome Safe Storage", "Chrome"),
            Browser::Chromium => ("Chromium Safe Storage", "Chromium"),
            Browser::Brave => ("Brave Safe Storage", "Brave"),
            Browser::Edge => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
        }
    }

    /// The `application` attribute of its libsecret item on Linux.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn libsecret_application(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Brave => "brave",
            Browser::Edge => "microsoft-edge",
        }
    }
}

/// The cookie database of a profile folder: `Network/Cookies` in current versions, `Cookies` in older ones.
pub fn cookies_file(profile_dir: &Path) -> PathBuf {
    let network = profile_dir.join("Network/Cookies");
    if network.exists() { network } else { profile_dir.join("Cookies") }
}

/// The AES keys to try, by value prefix.
#[derive(Debug, Clone, Default)]
pub struct Keys {
    pub v10: Vec<[u8; 16]>,
    pub v11: Vec<[u8; 16]>,
}

pub fn derive(password: &[u8], iterations: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", iterations, &mut key);
    key
}

impl Keys {
    /// macOS: the Keychain password.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn mac(password: &[u8]) -> Keys {
        Keys { v10: vec![derive(password, 1003)], v11: vec![] }
    }

    /// Linux: `peanuts` for `v10`; for `v11` the keyring's password when there is one, then the empty password (what
    /// the browser uses when its keyring gave it nothing).
    pub fn linux(keyring: Option<&[u8]>) -> Keys {
        let mut v11 = vec![];
        if let Some(pw) = keyring {
            v11.push(derive(pw, 1));
        }
        v11.push(derive(b"", 1));
        Keys { v10: vec![derive(b"peanuts", 1)], v11 }
    }
}

const IV: [u8; 16] = [b' '; 16];

fn aes_cbc_decrypt(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), &IV.into()).decrypt_padded_vec_mut::<Pkcs7>(data).ok()
}

/// One cookie's value: the plaintext `value` column when `encrypted` is empty, else `encrypted` opened with the first
/// key that works (the padding is right, the host hash matches when `hashed`, and the result is text).
pub fn decrypt(host_key: &str, plain: &str, encrypted: &[u8], keys: &Keys, hashed: bool) -> Result<String, String> {
    if encrypted.is_empty() {
        return Ok(plain.to_string());
    }
    let (prefix, data) = encrypted.split_at(encrypted.len().min(3));
    let candidates = match prefix {
        b"v10" => &keys.v10,
        b"v11" => &keys.v11,
        b"v20" => return Err("app-bound encryption (v20), which only the browser itself can open".into()),
        _ => return Err("an encryption format this plugin doesn't know".into()),
    };
    if candidates.is_empty() {
        return Err(format!("{} values, which aren't used on this system", String::from_utf8_lossy(prefix)));
    }
    for key in candidates {
        let Some(mut out) = aes_cbc_decrypt(key, data) else { continue };
        if hashed {
            let digest = Sha256::digest(host_key.as_bytes());
            if out.len() < 32 || out[..32] != digest[..] {
                continue;
            }
            out.drain(..32);
        }
        if let Ok(s) = String::from_utf8(out) {
            return Ok(s);
        }
    }
    Err("the browser's key doesn't open it".into())
}

/// Microseconds since 1601-01-01 (the browser's clock) to Unix seconds; 0 is "no expiry".
pub fn chrome_time_to_unix(us: i64) -> Option<i64> {
    (us > 0).then(|| us / 1_000_000 - 11_644_473_600)
}

/// The cookies of `domains` in the database at `db`, decrypted with `keys`. The file (and its journal) is copied to a
/// private temporary folder first, so a browser that's running and holds a lock on it doesn't matter.
pub fn read(db: &Path, keys: &Keys, domains: &[String]) -> anyhow::Result<Vec<Cookie>> {
    anyhow::ensure!(db.exists(), "there's no cookie database at {}", db.display());
    let tmp = tempfile::tempdir()?;
    let copy = tmp.path().join("Cookies");
    std::fs::copy(db, &copy)?;
    for suffix in ["-wal", "-journal"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            std::fs::copy(&side, tmp.path().join(format!("Cookies{suffix}")))?;
        }
    }
    let conn = rusqlite::Connection::open(&copy)?;
    let version: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let hashed = version >= 24;
    let mut stmt = conn.prepare(
        "SELECT host_key, name, value, encrypted_value, path, expires_utc, has_expires, is_secure, is_httponly, samesite
         FROM cookies",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Vec<u8>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
        ))
    })?;
    let mut out = vec![];
    let mut failed: Vec<String> = vec![];
    for row in rows {
        let (host, name, plain, enc, path, expires, has_expires, secure, http_only, same_site) = row?;
        if !in_domains(&host, domains) {
            continue;
        }
        match decrypt(&host, &plain, &enc, keys, hashed) {
            Ok(value) => out.push(Cookie {
                domain: host,
                name,
                value,
                path,
                expires: if has_expires == 0 { None } else { chrome_time_to_unix(expires) },
                secure: secure != 0,
                http_only: http_only != 0,
                same_site: match same_site {
                    0 => Some("None".into()),
                    1 => Some("Lax".into()),
                    2 => Some("Strict".into()),
                    _ => None,
                },
            }),
            Err(why) => failed.push(format!("{name} on {host}: {why}")),
        }
    }
    if out.is_empty() && !failed.is_empty() {
        anyhow::bail!("no cookie could be decrypted ({})", failed.join("; "));
    }
    for f in &failed {
        tracing::warn!("left out the cookie {f}");
    }
    out.sort();
    Ok(out)
}

/// The keys for `browser` on this system. On macOS this reads the Keychain, which asks the person the first time
/// (and blocks until they answer); "Always Allow" lets a signed binary read it from then on.
pub fn keys(browser: Browser) -> anyhow::Result<Keys> {
    #[cfg(target_os = "macos")]
    {
        let (service, account) = browser.keychain_item();
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(pw) => Ok(Keys::mac(&pw)),
            Err(e) if e.code() == -25300 => anyhow::bail!(
                "the Keychain has no \"{service}\" item: is {} installed and has it been opened once?",
                browser.label()
            ),
            Err(e) if e.code() == -128 || e.code() == -25293 => anyhow::bail!(
                "macOS didn't let fleetlayer-cookies read \"{service}\" from the Keychain (refused or cancelled); \
                 restart the plugin and choose Always Allow"
            ),
            Err(e) => anyhow::bail!("reading \"{service}\" from the Keychain: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Keys::linux(libsecret_password(browser).as_deref()))
    }
}

/// The browser's password in the desktop keyring, through `secret-tool` (libsecret); None when there's no keyring,
/// no `secret-tool` or no item.
#[cfg(not(target_os = "macos"))]
fn libsecret_password(browser: Browser) -> Option<Vec<u8>> {
    let out = std::process::Command::new("secret-tool")
        .args(["lookup", "application", browser.libsecret_application()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut pw = out.stdout;
    while pw.last() == Some(&b'\n') {
        pw.pop();
    }
    (out.status.success() && !pw.is_empty()).then_some(pw)
}

#[cfg(test)]
pub mod fixture {
    //! Cookie databases made the way the browser makes them, for tests.
    use super::*;
    use aes::cipher::BlockEncryptMut;

    pub fn encrypt(key: &[u8; 16], prefix: &[u8], host: &str, value: &str, hashed: bool) -> Vec<u8> {
        let mut plain = vec![];
        if hashed {
            plain.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        plain.extend_from_slice(value.as_bytes());
        let mut out = prefix.to_vec();
        out.extend(cbc::Encryptor::<aes::Aes128>::new(key.into(), &IV.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain));
        out
    }

    pub struct Row<'a> {
        pub host: &'a str,
        pub name: &'a str,
        pub value: &'a str,
        pub expires_unix: Option<i64>,
        pub http_only: bool,
    }

    /// A database with the browser's schema (the columns this plugin reads, and a few it doesn't), at `meta` version
    /// `version`, its values encrypted with `key` and `prefix` (or left plain when `prefix` is empty).
    pub fn make(path: &Path, version: i64, key: &[u8; 16], prefix: &[u8], rows: &[Row]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
             CREATE TABLE cookies (creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL, top_frame_site_key TEXT NOT
             NULL DEFAULT '', name TEXT NOT NULL, value TEXT NOT NULL, encrypted_value BLOB NOT NULL DEFAULT x'',
             path TEXT NOT NULL, expires_utc INTEGER NOT NULL, is_secure INTEGER NOT NULL, is_httponly INTEGER NOT
             NULL, last_access_utc INTEGER NOT NULL DEFAULT 0, has_expires INTEGER NOT NULL DEFAULT 1, is_persistent
             INTEGER NOT NULL DEFAULT 1, priority INTEGER NOT NULL DEFAULT 1, samesite INTEGER NOT NULL DEFAULT -1,
             source_scheme INTEGER NOT NULL DEFAULT 0, source_port INTEGER NOT NULL DEFAULT -1);",
        )
        .unwrap();
        conn.execute("INSERT INTO meta VALUES ('version', ?1)", [version.to_string()]).unwrap();
        for r in rows {
            let hashed = version >= 24;
            let (plain, enc) =
                if prefix.is_empty() { (r.value, vec![]) } else { ("", encrypt(key, prefix, r.host, r.value, hashed)) };
            let expires = r.expires_unix.map(|e| (e + 11_644_473_600) * 1_000_000).unwrap_or(0);
            conn.execute(
                "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, path, expires_utc,
                 is_secure, is_httponly, has_expires, samesite) VALUES (1, ?1, ?2, ?3, ?4, '/', ?5, 1, ?6, ?7, 0)",
                rusqlite::params![
                    r.host,
                    r.name,
                    plain,
                    enc,
                    expires,
                    r.http_only as i64,
                    r.expires_unix.is_some() as i64
                ],
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    fn rows() -> Vec<Row<'static>> {
        vec![
            Row {
                host: ".x.com",
                name: "auth_token",
                value: "abc123",
                expires_unix: Some(2_000_000_000),
                http_only: true,
            },
            Row { host: ".x.com", name: "ct0", value: "def456", expires_unix: Some(1_900_000_000), http_only: false },
            Row { host: "api.x.com", name: "sub", value: "s", expires_unix: None, http_only: false },
            Row { host: ".example.com", name: "other", value: "nope", expires_unix: None, http_only: false },
            Row { host: ".notx.com", name: "lookalike", value: "nope", expires_unix: None, http_only: false },
        ]
    }

    fn x() -> Vec<String> {
        vec!["x.com".into()]
    }

    #[test]
    fn macos_v10_with_the_host_hash() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("Cookies");
        let keys = Keys::mac(b"keychain-password");
        make(&db, 24, &keys.v10[0], b"v10", &rows());
        let got = read(&db, &keys, &x()).unwrap();
        let names: Vec<_> = got.iter().map(|c| (c.domain.as_str(), c.name.as_str(), c.value.as_str())).collect();
        assert_eq!(names, [(".x.com", "auth_token", "abc123"), (".x.com", "ct0", "def456"), ("api.x.com", "sub", "s")]);
        assert_eq!(got[0].expires, Some(2_000_000_000));
        assert!(got[0].http_only && got[0].secure && !got[0].host_only());
        assert_eq!(got[0].same_site.as_deref(), Some("None"));
        assert_eq!(got[2].expires, None);
        assert!(got[2].host_only());
        // The wrong password opens nothing.
        let err = read(&db, &Keys::mac(b"wrong"), &x()).unwrap_err().to_string();
        assert!(err.contains("doesn't open it"), "{err}");
    }

    #[test]
    fn older_databases_have_no_host_hash() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("Cookies");
        let keys = Keys::mac(b"pw");
        make(&db, 23, &keys.v10[0], b"v10", &rows());
        assert_eq!(read(&db, &keys, &x()).unwrap()[0].value, "abc123");
    }

    #[test]
    fn linux_v10_and_v11() {
        let d = tempfile::tempdir().unwrap();
        let v10 = d.path().join("v10");
        make(&v10, 24, &derive(b"peanuts", 1), b"v10", &rows());
        assert_eq!(read(&v10, &Keys::linux(None), &x()).unwrap()[1].value, "def456");
        let v11 = d.path().join("v11");
        make(&v11, 24, &derive(b"from-the-keyring", 1), b"v11", &rows());
        assert_eq!(read(&v11, &Keys::linux(Some(b"from-the-keyring")), &x()).unwrap()[1].value, "def456");
        assert!(read(&v11, &Keys::linux(None), &x()).is_err(), "without the keyring a v11 value can't be opened");
        // A browser whose keyring gave it nothing encrypts v11 with the empty password.
        let empty = d.path().join("empty");
        make(&empty, 24, &derive(b"", 1), b"v11", &rows());
        assert_eq!(read(&empty, &Keys::linux(None), &x()).unwrap()[0].value, "abc123");
    }

    #[test]
    fn plain_values_and_app_bound_ones() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("plain");
        make(&db, 24, &[0; 16], b"", &rows());
        assert_eq!(read(&db, &Keys::default(), &x()).unwrap()[0].value, "abc123");
        let err = decrypt(".x.com", "", b"v20whatever", &Keys::linux(None), true).unwrap_err();
        assert!(err.contains("app-bound"), "{err}");
    }

    #[test]
    fn domains_match_the_host_and_its_subdomains_only() {
        let d = vec!["x.com".to_string()];
        assert!(in_domains(".x.com", &d));
        assert!(in_domains("x.com", &d));
        assert!(in_domains("api.X.com", &d));
        assert!(!in_domains(".notx.com", &d));
        assert!(!in_domains("x.com.evil.net", &d));
        assert!(!in_domains(".com", &d));
    }

    /// The Keychain path, against CI's throwaway default keychain (the item is made readable by any program, so no
    /// prompt shows): `cargo test -p fleetlayer-cookies -- --ignored real_keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes to the default keychain: run it only where that's a throwaway one"]
    fn real_keychain_gives_the_v10_key() {
        let ok = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-A",
                "-s",
                "Chrome Safe Storage",
                "-a",
                "Chrome",
                "-w",
                "ci-password",
            ])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let keys = keys(Browser::Chrome).unwrap();
        assert_eq!(keys.v10, vec![derive(b"ci-password", 1003)]);
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("Cookies");
        make(&db, 24, &keys.v10[0], b"v10", &rows());
        assert_eq!(read(&db, &keys, &x()).unwrap()[0].value, "abc123");
        let err = super::keys(Browser::Brave).unwrap_err().to_string();
        assert!(err.contains("no \"Brave Safe Storage\" item"), "{err}");
    }

    #[test]
    fn chrome_time() {
        assert_eq!(chrome_time_to_unix(0), None);
        assert_eq!(chrome_time_to_unix(13_253_932_800_000_000), Some(1_609_459_200));
    }
}
