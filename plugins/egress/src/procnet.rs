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

/// One socket in a /proc/net/{tcp,tcp6,udp,udp6} table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    /// The kernel's state: 01 is ESTABLISHED for TCP; 07 is an open UDP socket.
    pub state: u8,
    pub uid: u32,
    /// 0 for a socket no process holds any more (TIME_WAIT).
    pub inode: u64,
}

pub fn rows(table: &str) -> Vec<Row> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some(Row {
                local: addr(f.get(1)?)?,
                remote: addr(f.get(2)?)?,
                state: u8::from_str_radix(f.get(3)?, 16).ok()?,
                uid: f.get(7)?.parse().ok()?,
                inode: f.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && unmap(a.ip()) == unmap(b.ip())
}

/// The one owner of the live sockets in `rows`, or None when there's none, or more than one (can't tell).
fn only_owner<'a>(rows: impl Iterator<Item = &'a Row>) -> Option<&'a Row> {
    let mut found: Option<&Row> = None;
    for r in rows {
        match found {
            Some(f) if f.uid != r.uid => return None,
            Some(_) => {}
            None => found = Some(r),
        }
    }
    found
}

/// The ESTABLISHED TCP socket whose local address is `peer` (the client end of a connection we accepted), connected
/// to `to` when given (our own listener, for a connection made to it directly). A closed socket (TIME_WAIT, inode 0,
/// shown as uid 0) is never an owner, and two live sockets on that address with different owners mean nobody.
pub fn tcp_owner_to(tables: &[String], peer: SocketAddr, to: Option<SocketAddr>) -> Option<Owner> {
    let rows: Vec<Row> = tables.iter().flat_map(|t| rows(t)).collect();
    let live = rows
        .iter()
        .filter(|r| same(r.local, peer) && r.state == 0x01 && r.inode != 0 && to.is_none_or(|t| same(r.remote, t)));
    only_owner(live).map(|r| Owner { uid: r.uid, remote: r.remote })
}

#[cfg(test)]
pub fn tcp_owner(tables: &[String], peer: SocketAddr) -> Option<Owner> {
    tcp_owner_to(tables, peer, None)
}

/// The UDP socket that sent from `peer` (bound to that address or to every address, on that port).
pub fn udp_owner(tables: &[String], peer: SocketAddr) -> Option<u32> {
    let rows: Vec<Row> = tables.iter().flat_map(|t| rows(t)).filter(|r| r.inode != 0).collect();
    only_owner(rows.iter().filter(|r| same(r.local, peer)))
        .or_else(|| only_owner(rows.iter().filter(|r| r.local.port() == peer.port() && r.local.ip().is_unspecified())))
        .map(|r| r.uid)
}

fn read(names: &[&str]) -> Vec<String> {
    names.iter().filter_map(|n| std::fs::read_to_string(format!("/proc/net/{n}")).ok()).collect()
}

pub fn tcp(peer: SocketAddr, to: Option<SocketAddr>) -> Option<Owner> {
    tcp_owner_to(&read(&["tcp", "tcp6"]), peer, to)
}

pub fn udp(peer: SocketAddr) -> Option<u32> {
    udp_owner(&read(&["udp", "udp6"]), peer)
}

/// Unix user names by uid, from /etc/passwd, and the subordinate uid ranges each user may map into a user namespace
/// (/etc/subuid): a process under one of those (rootless podman, `podman unshare`) is counted as that user.
#[derive(Default)]
pub struct Users {
    by_uid: HashMap<u32, String>,
    sub: Vec<(String, u32, u32)>,
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

    /// /etc/subuid's ranges as (user name, first, count); an owner written as a number is looked up in `by_uid`.
    pub fn parse_sub(subuid: &str, by_uid: &HashMap<u32, String>) -> Vec<(String, u32, u32)> {
        subuid
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.trim().split(':').collect();
                let owner = f.first()?.to_string();
                let owner = match owner.parse::<u32>() {
                    Ok(uid) => by_uid.get(&uid)?.clone(),
                    Err(_) => owner,
                };
                Some((owner, f.get(1)?.parse().ok()?, f.get(2)?.parse().ok()?))
            })
            .collect()
    }

    pub fn from_files(passwd: &str, subuid: &str) -> Users {
        let by_uid = Self::parse(passwd);
        let sub = Self::parse_sub(subuid, &by_uid);
        Users { by_uid, sub, read_at: Some(std::time::Instant::now()) }
    }

    #[cfg(test)]
    pub fn lookup_only(&self, uid: u32) -> Option<String> {
        self.lookup(uid)
    }

    fn lookup(&self, uid: u32) -> Option<String> {
        if let Some(n) = self.by_uid.get(&uid) {
            return Some(n.clone());
        }
        self.sub.iter().find(|(_, first, n)| uid >= *first && uid - first < *n).map(|(o, _, _)| o.clone())
    }

    /// The user `uid` is, or whose subordinate uid it is; None for a uid nobody owns. Reads the files again at most
    /// once a minute when it's unknown.
    pub fn name(&mut self, uid: u32) -> Option<String> {
        if let Some(n) = self.lookup(uid) {
            return Some(n);
        }
        if self.read_at.is_none_or(|t| t.elapsed().as_secs() > 60) {
            let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
            let subuid = std::fs::read_to_string("/etc/subuid").unwrap_or_default();
            *self = Self::from_files(&passwd, &subuid);
        }
        self.lookup(uid)
    }

    pub fn uid_of(passwd: &str, name: &str) -> Option<u32> {
        Self::parse(passwd).into_iter().find(|(_, n)| n == name).map(|(u, _)| u)
    }

    /// A user's uid and subordinate ranges (first, count), for the firewall.
    pub fn ids(passwd: &str, subuid: &str, name: &str) -> Option<(u32, Vec<(u32, u32)>)> {
        let by_uid = Self::parse(passwd);
        let uid = by_uid.iter().find(|(_, n)| *n == name).map(|(u, _)| *u)?;
        let ranges = Self::parse_sub(subuid, &by_uid)
            .into_iter()
            .filter(|(o, _, _)| o == name)
            .map(|(_, f, n)| (f, n))
            .collect();
        Some((uid, ranges))
    }
}
