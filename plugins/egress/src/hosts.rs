//! Host names, the patterns that match them, and the address ranges a restricted role stays off by default.

use std::net::{IpAddr, Ipv6Addr};

/// A host and port a connection asks for, normalised: lower case, no trailing dot, IPv6 without brackets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl Target {
    /// `host:<name>` or `host:<name>:<port>` for access control: the port only when it isn't 443 or 80.
    pub fn policy_name(&self) -> String {
        if self.port == 443 || self.port == 80 {
            format!("host:{}", self.display_host())
        } else {
            format!("host:{}:{}", self.display_host(), self.port)
        }
    }

    pub fn display_host(&self) -> String {
        if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() }
    }

    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.display_host(), self.port)
    }
}

/// A host name or IP address, normalised; None when it isn't one.
pub fn normalise_host(h: &str) -> Option<String> {
    let h = h.trim().trim_end_matches('.');
    let h = h.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(h);
    if let Ok(ip) = h.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    let lower = h.to_ascii_lowercase();
    valid_name(&lower).then_some(lower)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
}

/// `host`, `host:port`, `[v6]:port` or a bare IPv6 address, with `default_port` when there's none.
pub fn parse_authority(s: &str, default_port: u16) -> Option<Target> {
    let s = s.trim();
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (v6, after) = rest.split_once(']')?;
        let port = match after {
            "" => default_port,
            p => p.strip_prefix(':')?.parse().ok()?,
        };
        (v6.to_string(), port)
    } else if s.parse::<Ipv6Addr>().is_ok() {
        (s.to_string(), default_port)
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (s.to_string(), default_port),
        }
    };
    if port == 0 {
        return None;
    }
    Some(Target { host: normalise_host(&host)?, port })
}

/// One allowlist entry: `example.com` (that name), `*.example.com` (any name under it, not itself), an IP address,
/// each with an optional `:port`. Without a port it allows 443 and 80.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// The name after any `*.`.
    pub name: String,
    pub wildcard: bool,
    pub port: Option<u16>,
}

impl Pattern {
    pub fn parse(s: &str) -> Result<Pattern, String> {
        let raw = s.trim();
        let (wildcard, rest) = match raw.strip_prefix("*.") {
            Some(r) => (true, r),
            None => (false, raw),
        };
        if rest.contains('*') {
            return Err(format!("{raw:?}: a * goes only at the start, as *.example.com"));
        }
        let explicit_port = rest.starts_with('[') && rest.contains("]:")
            || (rest.parse::<Ipv6Addr>().is_err() && !rest.starts_with('[') && rest.contains(':'));
        let t = parse_authority(rest, 443).ok_or_else(|| format!("{raw:?} isn't a host name, *.name or IP address"))?;
        if wildcard && t.ip().is_some() {
            return Err(format!("{raw:?}: *. goes with a name, not an address"));
        }
        if !wildcard && !t.host.contains('.') && t.ip().is_none() && t.host != "localhost" {
            return Err(format!("{raw:?}: a bare name with no dot would match a local name; write the full name"));
        }
        Ok(Pattern { name: t.host, wildcard, port: explicit_port.then_some(t.port) })
    }

    pub fn matches(&self, t: &Target) -> bool {
        let port_ok = match self.port {
            Some(p) => p == t.port,
            None => t.port == 443 || t.port == 80,
        };
        port_ok && self.matches_name(&t.host)
    }

    /// Whether the name matches, whatever the port (DNS asks only about names).
    pub fn matches_name(&self, host: &str) -> bool {
        if self.wildcard {
            host.len() > self.name.len() + 1
                && host.ends_with(&self.name)
                && host.as_bytes()[host.len() - self.name.len() - 1] == b'.'
        } else {
            host == self.name
        }
    }

    /// Exact entries (no `*.`) may reach private addresses: the person named that host on purpose.
    pub fn is_exact(&self) -> bool {
        !self.wildcard
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let host = if self.name.contains(':') { format!("[{}]", self.name) } else { self.name.clone() };
        let star = if self.wildcard { "*." } else { "" };
        match self.port {
            Some(p) => write!(f, "{star}{host}:{p}"),
            None => write!(f, "{star}{host}"),
        }
    }
}

/// An address range (`10.0.0.0/8`, `fd00::/8`, or one address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let (a, p) = match s.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s.trim(), None),
        };
        let addr: IpAddr = a.parse().map_err(|_| format!("{s:?} isn't an address range like 10.0.0.0/8"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(|| format!("{s:?}: bad prefix"))?,
            None => max,
        };
        Ok(Cidr { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, unmap(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// An IPv4-mapped IPv6 address as the IPv4 address it carries.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// Addresses that aren't the public internet: loopback, private networks (the LAN), carrier-grade NAT (where
/// Tailscale puts a tailnet), link-local (cloud metadata), unique-local IPv6, multicast and reserved ranges.
pub fn is_private(ip: IpAddr) -> bool {
    match unmap(ip) {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg & 0xfe00) == 0xfc00
                || (seg & 0xffc0) == 0xfe80
        }
    }
}
