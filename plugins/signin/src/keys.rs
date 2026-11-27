//! The token-signing keys: RS256 (every OpenID Connect client must support it), kept in `keys/` and published in the
//! JWKS. Whoever holds one can sign in as anyone to every service, so they never leave the sign-in service's folder.
//!
//! - `signing.pem` signs every new token; a new one is made every 90 days ([`ROTATE_AFTER_MS`]).
//! - `previous.pem`, the one before, stays in the JWKS for two weeks ([`KEEP_PREVIOUS_MS`]) so tokens it signed
//!   still verify, then is deleted.
//! - `state.json` says when each was made and retired.
//!
//! `fleetlayer-signin keys rotate` rotates now, and `--drop-previous` (for a key that leaked) takes the old key out
//! of the JWKS at once. The running service notices changed files within a minute ([`Keys::refresh`]).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use rsa::{PrivateKey as RsaPrivateKey, PublicKey as RsaPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DAY_MS: i64 = 86_400_000;
/// A new signing key this often.
pub const ROTATE_AFTER_MS: i64 = 90 * DAY_MS;
/// The previous key stays in the JWKS this long after it stops signing.
pub const KEEP_PREVIOUS_MS: i64 = 14 * DAY_MS;

struct Key {
    key: RsaPrivateKey,
    /// The key's ID in tokens and the JWKS: from its public key, so a copied folder keeps it.
    kid: String,
}

impl Key {
    fn new(key: RsaPrivateKey) -> Key {
        let kid = kid_of(&key.to_public_key());
        Key { key, kid }
    }

    fn jwk(&self) -> Value {
        let public = self.key.to_public_key();
        json!({
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": self.kid,
            "n": B64.encode(public.n_bytes()), "e": B64.encode(public.e_bytes()),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct State {
    /// When `signing.pem` was made.
    made_at: i64,
    /// When `previous.pem` stopped signing; none when there's no previous key.
    #[serde(default)]
    previous_retired_at: Option<i64>,
}

struct Loaded {
    current: Key,
    previous: Option<Key>,
    state: State,
}

pub struct Keys {
    dir: PathBuf,
    loaded: RwLock<Loaded>,
}

fn kid_of(public: &RsaPublicKey) -> String {
    let n = public.n_bytes();
    hex::encode(&Sha256::digest(&n)[..8])
}

fn read_key(path: &Path) -> anyhow::Result<Option<RsaPrivateKey>> {
    match std::fs::read_to_string(path) {
        Ok(pem) => {
            Ok(Some(RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    }
}

fn write_key(path: &Path, key: &RsaPrivateKey) -> anyhow::Result<()> {
    crate::store::write_private(path, key.to_pkcs8_pem(LineEnding::LF)?.as_bytes())
}

fn new_key() -> anyhow::Result<RsaPrivateKey> {
    Ok(RsaPrivateKey::new(&mut crypto_common::getrandom::SysRng, 2048)?)
}

impl Keys {
    fn path(dir: &Path, name: &str) -> PathBuf {
        dir.join("keys").join(name)
    }

    fn read(dir: &Path, now: i64) -> anyhow::Result<Loaded> {
        let signing = Self::path(dir, "signing.pem");
        let state_path = Self::path(dir, "state.json");
        std::fs::create_dir_all(dir.join("keys"))?;
        let current = match read_key(&signing)? {
            Some(k) => k,
            None => {
                let key = new_key()?;
                write_key(&signing, &key)?;
                tracing::info!("made a new token-signing key in {}", signing.display());
                crate::store::write_private(
                    &state_path,
                    &serde_json::to_vec(&State { made_at: now, ..State::default() })?,
                )?;
                key
            }
        };
        let state: State = match std::fs::read(&state_path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("{}: {e}", state_path.display()))?,
            // A folder from before rotation: its key counts as made now.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let s = State { made_at: now, ..State::default() };
                crate::store::write_private(&state_path, &serde_json::to_vec(&s)?)?;
                s
            }
            Err(e) => anyhow::bail!("{}: {e}", state_path.display()),
        };
        let previous = match state.previous_retired_at {
            Some(_) => read_key(&Self::path(dir, "previous.pem"))?.map(Key::new),
            None => None,
        };
        Ok(Loaded { current: Key::new(current), previous, state })
    }

    /// The keys in `dir/keys/`, made if there are none.
    pub fn load_or_make(dir: &Path) -> anyhow::Result<Keys> {
        let loaded = Self::read(dir, crate::store::now_ms())?;
        Ok(Keys { dir: dir.to_path_buf(), loaded: RwLock::new(loaded) })
    }

    /// The ID of the key signing new tokens.
    #[cfg(test)]
    pub fn kid(&self) -> String {
        self.loaded.read().expect("keys lock").current.kid.clone()
    }

    /// A signed JWT (RS256) carrying `claims`, with the current key.
    pub fn sign(&self, claims: &Value) -> anyhow::Result<String> {
        self.sign_typed("JWT", claims)
    }

    /// The same, with its own `typ` (`logout+jwt` for a logout token, so it can't pass for an ID token).
    pub fn sign_typed(&self, typ: &str, claims: &Value) -> anyhow::Result<String> {
        let (key, kid) = {
            let l = self.loaded.read().expect("keys lock");
            (l.current.key.clone(), l.current.kid.clone())
        };
        let header = json!({"alg": "RS256", "typ": typ, "kid": kid});
        let input = format!("{}.{}", B64.encode(serde_json::to_vec(&header)?), B64.encode(serde_json::to_vec(claims)?));
        let signer = SigningKey::<Sha256>::new(key);
        let sig = signer.sign(input.as_bytes());
        Ok(format!("{input}.{}", B64.encode(sig.to_bytes())))
    }

    /// The JWKS: the current key and, for two weeks after a rotation, the previous one.
    pub fn jwks(&self) -> Value {
        let l = self.loaded.read().expect("keys lock");
        let keys: Vec<Value> = std::iter::once(&l.current).chain(l.previous.as_ref()).map(Key::jwk).collect();
        json!({ "keys": keys })
    }

    /// Make a new signing key now. The current one becomes the previous (kept two weeks), or, with `drop_previous`,
    /// leaves the JWKS at once: every token it signed stops verifying, so services sign people in again.
    pub fn rotate_files(dir: &Path, now: i64, drop_previous: bool) -> anyhow::Result<String> {
        let loaded = Self::read(dir, now)?;
        let key = new_key()?;
        let previous = Self::path(dir, "previous.pem");
        let state = if drop_previous {
            match std::fs::remove_file(&previous) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!("{}: {e}", previous.display()),
            }
            State { made_at: now, previous_retired_at: None }
        } else {
            write_key(&previous, &loaded.current.key)?;
            State { made_at: now, previous_retired_at: Some(now) }
        };
        // In this order, a crash part way leaves a folder that loads: at worst the old key again, rotated next time.
        write_key(&Self::path(dir, "signing.pem"), &key)?;
        crate::store::write_private(&Self::path(dir, "state.json"), &serde_json::to_vec(&state)?)?;
        let kid = kid_of(&key.to_public_key());
        tracing::info!(kid, drop_previous, "rotated the token-signing key");
        Ok(kid)
    }

    /// Run every minute: rotate when the key is 90 days old, drop the previous key after two weeks, and pick up a
    /// rotation made by `fleetlayer-signin keys rotate` while the service was running. Generating a key is slow, so
    /// call it from a blocking task.
    pub fn refresh(&self, now: i64) -> anyhow::Result<()> {
        let state = self.loaded.read().expect("keys lock").state.clone();
        if now - state.made_at >= ROTATE_AFTER_MS {
            Self::rotate_files(&self.dir, now, false)?;
        } else if let Some(at) = state.previous_retired_at
            && now - at >= KEEP_PREVIOUS_MS
        {
            let previous = Self::path(&self.dir, "previous.pem");
            crate::store::write_private(
                &Self::path(&self.dir, "state.json"),
                &serde_json::to_vec(&State { made_at: state.made_at, previous_retired_at: None })?,
            )?;
            if let Err(e) = std::fs::remove_file(&previous)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                anyhow::bail!("{}: {e}", previous.display());
            }
            tracing::info!("the previous token-signing key left the JWKS");
        }
        let on_disk: State = serde_json::from_slice(&std::fs::read(Self::path(&self.dir, "state.json"))?)?;
        if on_disk != state {
            *self.loaded.write().expect("keys lock") = Self::read(&self.dir, now)?;
        }
        Ok(())
    }
}
/// Check a JWT against a JWKS and return its claims: what a service does with the ID token (used by the tests).
#[cfg(test)]
pub fn verify(jwks: &Value, token: &str) -> anyhow::Result<Value> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    let mut parts = token.split('.');
    let (h, c, s) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let header: Value = serde_json::from_slice(&B64.decode(h)?)?;
    let key = jwks["keys"]
        .as_array()
        .and_then(|ks| ks.iter().find(|k| k["kid"] == header["kid"]))
        .ok_or_else(|| anyhow::anyhow!("no key {} in the JWKS", header["kid"]))?;
    let n = rsa::BoxedUint::from_be_slice_vartime(&B64.decode(key["n"].as_str().unwrap_or(""))?);
    let e = rsa::BoxedUint::from_be_slice_vartime(&B64.decode(key["e"].as_str().unwrap_or(""))?);
    let public = RsaPublicKey::new(n, e)?;
    let sig = Signature::try_from(B64.decode(s)?.as_slice())?;
    VerifyingKey::<Sha256>::new(public).verify(format!("{h}.{c}").as_bytes(), &sig)?;
    Ok(serde_json::from_slice(&B64.decode(c)?)?)
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    fn kids(k: &Keys) -> Vec<String> {
        k.jwks()["keys"].as_array().unwrap().iter().map(|j| j["kid"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_key_is_replaced_after_90_days_and_the_old_one_verifies_for_two_weeks() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::load_or_make(dir.path()).unwrap();
        let now = crate::store::now_ms();
        let first = keys.kid();
        let old_token = keys.sign(&json!({"sub": "p_a"})).unwrap();

        keys.refresh(now + ROTATE_AFTER_MS - DAY_MS).unwrap();
        assert_eq!(keys.kid(), first, "not due yet");

        let rotated_at = now + ROTATE_AFTER_MS;
        keys.refresh(rotated_at).unwrap();
        assert_ne!(keys.kid(), first);
        assert_eq!(kids(&keys), vec![keys.kid(), first.clone()]);
        assert!(verify(&keys.jwks(), &old_token).is_ok(), "tokens from before still verify");
        let new_token = keys.sign(&json!({"sub": "p_a"})).unwrap();
        assert!(verify(&keys.jwks(), &new_token).is_ok());

        // A restart in between keeps both.
        let again = Keys::load_or_make(dir.path()).unwrap();
        assert_eq!(kids(&again), kids(&keys));

        keys.refresh(rotated_at + KEEP_PREVIOUS_MS).unwrap();
        assert_eq!(kids(&keys), vec![keys.kid()]);
        assert!(verify(&keys.jwks(), &old_token).is_err());
        assert!(!dir.path().join("keys/previous.pem").exists());
    }

    #[test]
    fn the_rotate_command_reaches_the_running_service_and_drop_previous_ends_old_tokens_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::load_or_make(dir.path()).unwrap();
        let now = crate::store::now_ms();
        let first = keys.kid();
        let old_token = keys.sign(&json!({"sub": "p_a"})).unwrap();

        let second = Keys::rotate_files(dir.path(), now, false).unwrap();
        keys.refresh(now).unwrap();
        assert_eq!(keys.kid(), second);
        assert_eq!(kids(&keys), vec![second.clone(), first]);
        assert!(verify(&keys.jwks(), &old_token).is_ok());

        // A leaked key: gone from the JWKS straight away, with everything it signed.
        let second_token = keys.sign(&json!({"sub": "p_a"})).unwrap();
        let third = Keys::rotate_files(dir.path(), now + 1, true).unwrap();
        keys.refresh(now + 1).unwrap();
        assert_eq!(kids(&keys), vec![third]);
        assert!(verify(&keys.jwks(), &second_token).is_err());
        assert!(verify(&keys.jwks(), &old_token).is_err());
    }

    #[test]
    fn a_folder_from_before_rotation_keeps_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = Keys::load_or_make(dir.path()).unwrap().kid();
        std::fs::remove_file(dir.path().join("keys/state.json")).unwrap();
        let keys = Keys::load_or_make(dir.path()).unwrap();
        assert_eq!(keys.kid(), first);
        keys.refresh(crate::store::now_ms() + DAY_MS).unwrap();
        assert_eq!(keys.kid(), first, "counted from when it was first seen, not rotated at once");
    }
}
