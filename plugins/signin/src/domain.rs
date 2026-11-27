//! Which domain passkeys are made for (the WebAuthn relying party ID). A passkey belongs to that domain, not to the
//! machine, so the default is the issuer's registrable domain (from the Public Suffix List): passkeys made at
//! `signin.home.example.com` are for `example.com` and keep working when sign-in moves anywhere under it, and a
//! tailnet's `laptop.tail1234.ts.net` gives `tail1234.ts.net`. On `localhost` or a bare machine name there's no domain
//! to plan for, and the sign-in page says so before the first passkey is made.

use url::Url;

/// The relying party ID for an issuer, and whether passkeys made for it will stop working if sign-in moves.
pub fn passkey_domain(issuer: &Url, configured: Option<&str>) -> anyhow::Result<(String, bool)> {
    let host =
        issuer.host_str().ok_or_else(|| anyhow::anyhow!("the issuer {issuer} has no host"))?.to_ascii_lowercase();
    if let Some(d) = configured {
        let d = d.trim_matches('.').to_ascii_lowercase();
        anyhow::ensure!(
            host == d || host.ends_with(&format!(".{d}")),
            "passkey_domain {d} must be the issuer's host ({host}) or a domain above it"
        );
        return Ok((d.clone(), fixed(&d)));
    }
    if fixed(&host) {
        return Ok((host.clone(), true));
    }
    match psl::domain_str(&host) {
        Some(d) => Ok((d.to_string(), false)),
        None => Ok((host.clone(), true)),
    }
}

/// A name with no domain above it to plan for: localhost, an IP address, a bare machine name.
fn fixed(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::Ipv6Addr>().is_ok()
        || !host.contains('.')
}

/// Whether plain http is allowed for this issuer: only on this machine.
pub fn local(issuer: &Url) -> bool {
    matches!(issuer.host_str(), Some(h) if h == "localhost" || h.ends_with(".localhost") || h == "127.0.0.1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(issuer: &str) -> (String, bool) {
        passkey_domain(&Url::parse(issuer).unwrap(), None).unwrap()
    }

    #[test]
    fn passkeys_are_for_the_registrable_domain() {
        assert_eq!(d("https://signin.home.example.com"), ("example.com".into(), false));
        assert_eq!(d("https://laptop.tail1234.ts.net"), ("tail1234.ts.net".into(), false), "ts.net is a public suffix");
        assert_eq!(d("https://signin.example.co.uk"), ("example.co.uk".into(), false));
        assert_eq!(d("http://signin.localhost:7680"), ("signin.localhost".into(), true));
        assert_eq!(d("https://10.0.0.5"), ("10.0.0.5".into(), true));
        assert_eq!(d("https://homeserver"), ("homeserver".into(), true));
    }

    #[test]
    fn a_configured_domain_must_be_above_the_issuer() {
        let u = Url::parse("https://signin.home.example.com").unwrap();
        assert_eq!(passkey_domain(&u, Some("home.example.com")).unwrap(), ("home.example.com".into(), false));
        assert!(passkey_domain(&u, Some("other.com")).is_err());
    }
}
