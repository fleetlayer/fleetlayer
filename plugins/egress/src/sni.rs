//! The host name a TLS client asks for (the ClientHello's server_name extension), read without decrypting anything,
//! and the alert that refuses a connection in words a TLS client prints ("tlsv1 alert access denied").

/// What the bytes so far say.
#[derive(Debug, PartialEq, Eq)]
pub enum Sni {
    /// The server name.
    Name(String),
    /// A ClientHello with no server name (a client connecting to an address).
    NoName,
    /// Not complete yet: read more.
    NeedMore,
    /// Not a TLS ClientHello.
    NotTls,
}

/// The most bytes read looking for a ClientHello: a large one (post-quantum key shares) is a few kilobytes.
pub const MAX_HELLO: usize = 32 * 1024;

/// A fatal `access_denied` alert (TLS 1.0 record version, accepted by every client).
pub const ACCESS_DENIED_ALERT: [u8; 7] = [0x15, 0x03, 0x01, 0x00, 0x02, 0x02, 49];

pub fn parse(buf: &[u8]) -> Sni {
    // The handshake message may span several records: gather their payloads.
    let mut hs = Vec::new();
    let mut at = 0;
    loop {
        if buf.len() < at + 5 {
            return if buf.len() > at && buf[at] != 0x16 { Sni::NotTls } else { Sni::NeedMore };
        }
        if buf[at] != 0x16 || buf[at + 1] != 0x03 {
            return Sni::NotTls;
        }
        let len = u16::from_be_bytes([buf[at + 3], buf[at + 4]]) as usize;
        if len == 0 || len > 16_384 + 256 {
            return Sni::NotTls;
        }
        if buf.len() < at + 5 + len {
            return Sni::NeedMore;
        }
        hs.extend_from_slice(&buf[at + 5..at + 5 + len]);
        at += 5 + len;
        if hs.len() >= 4 {
            if hs[0] != 0x01 {
                return Sni::NotTls;
            }
            let body = u32::from_be_bytes([0, hs[1], hs[2], hs[3]]) as usize;
            if hs.len() >= 4 + body {
                return hello(&hs[4..4 + body]).unwrap_or(Sni::NotTls);
            }
            if 4 + body > MAX_HELLO {
                return Sni::NotTls;
            }
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.b.len() < n {
            return None;
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Some(h)
    }
    fn u8(&mut self) -> Option<usize> {
        self.take(1).map(|b| b[0] as usize)
    }
    fn u16(&mut self) -> Option<usize> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
    }
}

fn hello(body: &[u8]) -> Option<Sni> {
    let mut r = Reader { b: body };
    r.take(2 + 32)?; // version, random
    let n = r.u8()?;
    r.take(n)?; // session id
    let n = r.u16()?;
    r.take(n)?; // cipher suites
    let n = r.u8()?;
    r.take(n)?; // compression methods
    if r.b.is_empty() {
        return Some(Sni::NoName);
    }
    let n = r.u16()?;
    let mut ext = Reader { b: r.take(n)? };
    while !ext.b.is_empty() {
        let kind = ext.u16()?;
        let n = ext.u16()?;
        let data = ext.take(n)?;
        if kind != 0 {
            continue;
        }
        let mut list = Reader { b: data };
        let n = list.u16()?;
        let mut names = Reader { b: list.take(n)? };
        while !names.b.is_empty() {
            let name_type = names.u8()?;
            let n = names.u16()?;
            let name = names.take(n)?;
            if name_type == 0 {
                let s = std::str::from_utf8(name).ok()?;
                return Some(Sni::Name(s.to_string()));
            }
        }
        return Some(Sni::NoName);
    }
    Some(Sni::NoName)
}

/// A minimal ClientHello naming `host`, for tests.
#[cfg(test)]
pub fn client_hello(host: Option<&str>) -> Vec<u8> {
    let mut ext = vec![];
    if let Some(h) = host {
        let h = h.as_bytes();
        let mut sni = vec![];
        sni.extend_from_slice(&((h.len() + 3) as u16).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&(h.len() as u16).to_be_bytes());
        sni.extend_from_slice(h);
        ext.extend_from_slice(&0u16.to_be_bytes());
        ext.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni);
    }
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[7u8; 32]);
    body.push(0); // session id
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // one cipher suite
    body.extend_from_slice(&[0x01, 0x00]); // null compression
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut hs = vec![0x01];
    hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&body);
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}
