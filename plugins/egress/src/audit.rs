//! The audit log: one JSON line per connection and DNS question (who, which host, the decision, bytes, time), in a
//! file per day under the state directory, kept `keep_days`. And `suggest`: the allowlist a week of it asks for.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::settings::{PRESETS, Settings};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub at: Timestamp,
    /// The Unix user, or `uid <n>` when it has no name.
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `connect` (a CONNECT through the proxy), `http` (a plain HTTP request), `tls` (redirected HTTPS), `dns`, `ask`.
    pub via: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `allowed`, `denied` or `would_deny` (log mode).
    pub decision: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub up: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub down: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Line {
    pub fn refused(&self) -> bool {
        self.decision == "denied" || self.decision == "would_deny"
    }
}

pub struct Log {
    dir: PathBuf,
    keep_days: u32,
    pruned_on: Option<String>,
}

fn day(t: Timestamp) -> String {
    t.strftime("%Y-%m-%d").to_string()
}

impl Log {
    pub fn new(dir: PathBuf, keep_days: u32) -> Self {
        Log { dir, keep_days, pruned_on: None }
    }

    pub fn set_keep_days(&mut self, days: u32) {
        self.keep_days = days;
    }

    pub fn write(&mut self, line: &Line) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let today = day(line.at);
        if self.pruned_on.as_deref() != Some(&today) {
            self.prune(line.at);
            self.pruned_on = Some(today.clone());
        }
        let mut f =
            std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{today}.jsonl")))?;
        let mut s = serde_json::to_string(line).map_err(std::io::Error::other)?;
        s.push('\n');
        f.write_all(s.as_bytes())
    }

    fn prune(&self, now: Timestamp) {
        let cutoff = day(now - jiff::SignedDuration::from_hours(24 * self.keep_days as i64));
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(d) = name.strip_suffix(".jsonl")
                && d < cutoff.as_str()
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Every line of the last `days` days, oldest first.
pub fn read(dir: &Path, days: u32, now: Timestamp) -> Vec<Line> {
    let cutoff = now - jiff::SignedDuration::from_hours(24 * days as i64);
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    files.sort();
    let first = day(cutoff);
    let mut out = vec![];
    for f in files {
        let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if stem < first {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Line>(l).ok()).filter(|l| l.at >= cutoff));
    }
    out
}

#[derive(Default)]
struct Seen {
    connections: u64,
    dns_only: bool,
}

/// The settings each role's refusals ask for, as YAML to merge into the settings file, with counts as comments.
pub fn suggest(lines: &[Line], settings: &Settings, only_role: Option<&str>) -> String {
    // role -> host:port (or host) -> what was seen
    let mut by_role: BTreeMap<String, BTreeMap<String, Seen>> = BTreeMap::new();
    for l in lines.iter().filter(|l| l.refused() && l.via != "ask") {
        let Some(role) = &l.role else { continue };
        if only_role.is_some_and(|r| r != role) || l.host.ends_with(".arpa") || l.host.is_empty() {
            continue;
        }
        let key = match l.port {
            Some(p) if p != 443 && p != 80 => format!("{}:{p}", l.host),
            _ => l.host.clone(),
        };
        let e = by_role.entry(role.clone()).or_default().entry(key).or_insert(Seen { connections: 0, dns_only: true });
        if l.via == "dns" {
            continue;
        }
        e.connections += 1;
        e.dns_only = false;
    }
    let mut out = String::new();
    if by_role.is_empty() {
        out.push_str("# Nothing was refused (or would have been) in this time: no changes to suggest.\n");
        return out;
    }
    out.push_str(
        "# Hosts each role was refused, or would have been in log mode. Review before adding: an allowed host\n",
    );
    out.push_str("# is a way out for data too. Merge into the settings file's roles.\n");
    out.push_str("roles:\n");
    for (role, hosts) in by_role {
        let have: BTreeSet<String> =
            settings.role(&role).map(|r| r.presets.iter().cloned().collect()).unwrap_or_default();
        let mut presets: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut rest: BTreeMap<String, &Seen> = BTreeMap::new();
        for (h, seen) in &hosts {
            let t = crate::hosts::parse_authority(h, 443);
            let preset = t.as_ref().and_then(|t| {
                PRESETS.iter().find(|(name, _, pats)| {
                    !have.contains(*name)
                        && pats.iter().any(|p| crate::hosts::Pattern::parse(p).is_ok_and(|p| p.matches(t)))
                })
            });
            match preset {
                Some((name, _, _)) => presets.entry(name).or_default().push(format!("{h} ({})", seen.connections)),
                None => {
                    rest.insert(h.clone(), seen);
                }
            }
        }
        // Several names under one registrable domain become one `*.` entry.
        let mut by_domain: BTreeMap<String, Vec<(String, &Seen)>> = BTreeMap::new();
        for (h, seen) in rest {
            let (name, port) = match crate::hosts::parse_authority(&h, 443) {
                Some(t) if t.port != 443 => (t.host, Some(t.port)),
                Some(t) => (t.host, None),
                None => (h.clone(), None),
            };
            let domain = if name.parse::<std::net::IpAddr>().is_ok() || port.is_some() {
                h.clone()
            } else {
                psl::domain_str(&name).unwrap_or(&name).to_string()
            };
            by_domain.entry(domain).or_default().push((h, seen));
        }
        out.push_str(&format!("  {role}:\n"));
        if !presets.is_empty() {
            let mut all: Vec<&str> = have.iter().map(String::as_str).collect();
            all.extend(presets.keys());
            out.push_str(&format!("    presets: [{}]\n", all.join(", ")));
            for (p, hs) in &presets {
                out.push_str(&format!("    # {p} covers: {}\n", hs.join(", ")));
            }
        }
        out.push_str("    hosts:\n");
        for (domain, entries) in by_domain {
            let total: u64 = entries.iter().map(|(_, s)| s.connections).sum();
            let names: Vec<String> = entries.iter().map(|(h, _)| h.clone()).collect();
            let dns_only = entries.iter().all(|(_, s)| s.dns_only);
            let note = if dns_only { ", DNS only".to_string() } else { String::new() };
            if entries.len() >= 3 {
                out.push_str(&format!("      - \"*.{domain}\"   # {}: {total} connections{note}\n", names.join(", ")));
                if names.contains(&domain) {
                    out.push_str(&format!("      - \"{domain}\"\n"));
                }
            } else {
                for (h, s) in entries {
                    let ip = if crate::hosts::parse_authority(&h, 443).and_then(|t| t.ip()).is_some() {
                        ", an address: find the name that uses it"
                    } else {
                        ""
                    };
                    let note = if s.dns_only { ", DNS only" } else { "" };
                    out.push_str(&format!("      - \"{h}\"   # {} connections{note}{ip}\n", s.connections));
                }
            }
        }
    }
    out
}
