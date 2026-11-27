//! Which Unix user a local connection comes from, read from /proc/net (the same table `ss -e` reads). A client's
//! socket row lists its owner's uid and, for a connection the firewall redirected here, the address it first asked for.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::hosts::unmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    /// The address the client's socket is connected to: for a redirected connection, its original destination.
    pub remote: SocketAddr,
}

fn addr(hex: &str) -> Option<SocketAddr> {
    let (ip, port) = hex.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(ip, 16).ok()?.to_le_bytes())),
        32 => {
            let mut b = [0u8; 16];
            for (i, chunk) in b.chunks_mut(4).enumerate() {
                let w = u32::from_str_radix(&ip[i * 8..i * 8 + 8], 16).ok()?;
                chunk.copy_from_slice(&w.to_le_bytes());
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The rows of a /proc/net/{tcp,tcp6,udp,udp6} table: (local, remote, uid).
pub fn rows(table: &str) -> Vec<(SocketAddr, SocketAddr, u32)> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some((addr(f.get(1)?)?, addr(f.get(2)?)?, f.get(7)?.parse().ok()?))
        })
        .collect()
}

fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && unmap(a.ip()) == unmap(b.ip())
}

/// The TCP socket whose local address is `peer` (the client end of a connection we accepted).
pub fn tcp_owner(tables: &[String], peer: SocketAddr) -> Option<Owner> {
    tables.iter().flat_map(|t| rows(t)).find(|(l, _, _)| same(*l, peer)).map(|(_, r, uid)| Owner { uid, remote: r })
}

/// The UDP socket that sent from `peer` (bound to that address or to every address, on that port).
pub fn udp_owner(tables: &[String], peer: SocketAddr) -> Option<u32> {
    let rows: Vec<_> = tables.iter().flat_map(|t| rows(t)).collect();
    rows.iter()
        .find(|(l, _, _)| same(*l, peer))
        .or_else(|| rows.iter().find(|(l, _, _)| l.port() == peer.port() && l.ip().is_unspecified()))
        .map(|(_, _, uid)| *uid)
}

fn read(names: &[&str]) -> Vec<String> {
    names.iter().filter_map(|n| std::fs::read_to_string(format!("/proc/net/{n}")).ok()).collect()
}

pub fn tcp(peer: SocketAddr) -> Option<Owner> {
    tcp_owner(&read(&["tcp", "tcp6"]), peer)
}

pub fn udp(peer: SocketAddr) -> Option<u32> {
    udp_owner(&read(&["udp", "udp6"]), peer)
}

/// Unix user names by uid, from /etc/passwd.
#[derive(Default)]
pub struct Users {
    by_uid: HashMap<u32, String>,
    read_at: Option<std::time::Instant>,
}

impl Users {
    pub fn parse(passwd: &str) -> HashMap<u32, String> {
        passwd
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                Some((f.get(2)?.parse().ok()?, f.first()?.to_string()))
            })
            .collect()
    }

    /// The name of `uid`, reading /etc/passwd again at most once a minute when it's unknown.
    pub fn name(&mut self, uid: u32) -> Option<String> {
        if let Some(n) = self.by_uid.get(&uid) {
            return Some(n.clone());
        }
        if self.read_at.is_none_or(|t| t.elapsed().as_secs() > 60) {
            self.by_uid = Self::parse(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
            self.read_at = Some(std::time::Instant::now());
        }
        self.by_uid.get(&uid).cloned()
    }

    pub fn uid_of(passwd: &str, name: &str) -> Option<u32> {
        Self::parse(passwd).into_iter().find(|(_, n)| n == name).map(|(u, _)| u)
    }
}
