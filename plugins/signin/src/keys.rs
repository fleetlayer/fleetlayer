//! The token-signing key: RS256 (every OpenID Connect client must support it), made on first start in
//! `keys/signing.pem` and published in the JWKS. Whoever holds it can sign in as anyone to every service, so it never
//! leaves the sign-in service's folder.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use rsa::{PrivateKey as RsaPrivateKey, PublicKey as RsaPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct Keys {
    key: RsaPrivateKey,
    /// The key's ID in tokens and the JWKS: from its public key, so a copied folder keeps it.
    pub kid: String,
}

fn kid_of(public: &RsaPublicKey) -> String {
    let n = public.n_bytes();
    hex::encode(&Sha256::digest(&n)[..8])
}

impl Keys {
    /// The key in `dir/keys/signing.pem`, made if there's none.
    pub fn load_or_make(dir: &Path) -> anyhow::Result<Keys> {
        let path = dir.join("keys").join("signing.pem");
        let key = match std::fs::read_to_string(&path) {
            Ok(pem) => RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = RsaPrivateKey::new(&mut crypto_common::getrandom::SysRng, 2048)?;
                let pem = key.to_pkcs8_pem(LineEnding::LF)?;
                crate::store::write_private(&path, pem.as_bytes())?;
                tracing::info!("made a new token-signing key in {}", path.display());
                key
            }
            Err(e) => anyhow::bail!("{}: {e}", path.display()),
        };
        let kid = kid_of(&key.to_public_key());
        Ok(Keys { key, kid })
    }

    /// A signed JWT (RS256) carrying `claims`.
    pub fn sign(&self, claims: &Value) -> anyhow::Result<String> {
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": self.kid});
        let input = format!("{}.{}", B64.encode(serde_json::to_vec(&header)?), B64.encode(serde_json::to_vec(claims)?));
        let signer = SigningKey::<Sha256>::new(self.key.clone());
        let sig = signer.sign(input.as_bytes());
        Ok(format!("{input}.{}", B64.encode(sig.to_bytes())))
    }

    /// The JWKS: the public key, for services to check tokens.
    pub fn jwks(&self) -> Value {
        let public = self.key.to_public_key();
        json!({"keys": [{
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": self.kid,
            "n": B64.encode(public.n_bytes()), "e": B64.encode(public.e_bytes()),
        }]})
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
