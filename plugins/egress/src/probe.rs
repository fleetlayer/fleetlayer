//! The probe: a plugin on a restricted role's own node that checks, every few minutes and as that role's user, that
//! the egress rules really hold: the proxy answers and knows the role; an allowed host gets through; and once enforced,
//! a blocked host is refused, its name doesn't resolve, and a direct connection past the proxy fails. Whatever fails is
//! reported as the plugin's problem, so the node's health check (and its alerts) hear about it.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    /// The proxy (default 127.0.0.1:3128).
    #[serde(default = "default_proxy")]
    pub proxy: String,
    /// A host on the role's list, checked to get through (default api.anthropic.com).
    #[serde(default = "default_allowed")]
    pub allowed: String,
    /// A host not on it, checked to be refused (default example.com).
    #[serde(default = "default_blocked")]
    pub blocked: String,
    /// An address that answers on a port the proxy never sees, checked to be unreachable (default 1.1.1.1:853).
    #[serde(default = "default_direct")]
    pub direct: String,
    /// How often, in seconds (default 300).
    #[serde(default = "default_every")]
    pub every: u64,
}

fn default_proxy() -> String {
    "127.0.0.1:3128".into()
}
fn default_allowed() -> String {
    "api.anthropic.com".into()
}
fn default_blocked() -> String {
    "example.com".into()
}
fn default_direct() -> String {
    "1.1.1.1:853".into()
}
fn default_every() -> u64 {
    300
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub role: Option<String>,
    pub mode: Option<String>,
    pub checks: Vec<(String, bool, String)>,
}

impl Report {
    fn add(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.push((name.into(), ok, detail.into()));
    }

    /// The first failed check, as the plugin's problem.
    pub fn problem(&self) -> Option<String> {
        self.checks.iter().find(|(_, ok, _)| !ok).map(|(n, _, d)| format!("egress probe: {n}: {d}"))
    }
}

/// Send one HTTP request to the proxy and read the whole answer: (status, body).
pub async fn http(proxy: &str, request: &str) -> anyhow::Result<(u16, String)> {
    let mut s = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(proxy)).await??;
    s.write_all(request.as_bytes()).await?;
    let mut buf = Vec::new();
    let read = async {
        let mut chunk = [0u8; 8192];
        loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // A CONNECT's 200 has no body and the tunnel stays open: stop at the end of its head.
            if request.starts_with("CONNECT") && buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        anyhow::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(20), read).await??;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Ok((status, body))
}

pub async fn run_once(c: &ProbeConfig) -> Report {
    let mut r = Report::default();
    let status = http(&c.proxy, "GET /status HTTP/1.1\r\nHost: egress.fleetlayer\r\nConnection: close\r\n\r\n").await;
    let v: serde_json::Value = match status {
        Ok((200, body)) => serde_json::from_str(&body).unwrap_or_default(),
        Ok((code, _)) => {
            r.add("proxy", false, format!("{} answered {code} to /status", c.proxy));
            return r;
        }
        Err(e) => {
            r.add("proxy", false, format!("{} isn't answering: {e}", c.proxy));
            return r;
        }
    };
    r.role = v["role"].as_str().map(str::to_string);
    r.mode = v["mode"].as_str().map(str::to_string);
    let Some(role) = r.role.clone() else {
        r.add("role", false, format!("user {} isn't in any role of the egress settings", v["user"]));
        return r;
    };
    r.add("proxy", true, format!("answers, role {role}"));
    let connect = |host: &str| format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n");
    match http(&c.proxy, &connect(&c.allowed)).await {
        Ok((200, _)) => r.add("allowed host", true, format!("{} gets through", c.allowed)),
        Ok((code, body)) => r.add(
            "allowed host",
            false,
            format!("{} was answered {code}: {}", c.allowed, body.lines().next().unwrap_or("")),
        ),
        Err(e) => r.add("allowed host", false, format!("{}: {e}", c.allowed)),
    }
    if r.mode.as_deref() != Some("enforce") {
        return r;
    }
    match http(&c.proxy, &connect(&c.blocked)).await {
        Ok((403, _)) => r.add("blocked host", true, format!("{} is refused", c.blocked)),
        Ok((code, _)) => r.add("blocked host", false, format!("{} was answered {code}, not refused", c.blocked)),
        Err(e) => r.add("blocked host", false, format!("{}: {e}", c.blocked)),
    }
    let resolved = tokio::net::lookup_host((c.blocked.as_str(), 443)).await.map(|a| a.count()).unwrap_or(0);
    if resolved > 0 {
        r.add("dns", false, format!("{} resolves: DNS isn't filtered", c.blocked));
    } else {
        r.add("dns", true, format!("{} doesn't resolve", c.blocked));
    }
    match c.direct.parse::<SocketAddr>() {
        Ok(a) => match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(a)).await {
            Ok(Ok(_)) => {
                r.add("firewall", false, format!("a direct connection to {a} got through: the firewall isn't applied"))
            }
            // Refused at once (the firewall's reset) or dropped: either way it didn't leave.
            _ => r.add("firewall", true, format!("a direct connection to {a} fails")),
        },
        Err(_) => r.add("firewall", false, format!("direct: {:?} isn't an address:port", c.direct)),
    }
    if let Some((ok, detail)) = subordinate_check(&c.direct).await {
        r.add("subordinate uids", ok, detail);
    }
    r
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// A process under one of the role's subordinate uids (what rootless podman runs containers as) must be refused too.
/// Checked through `podman unshare` when podman is installed and the user has a range in /etc/subuid; None otherwise.
async fn subordinate_check(direct: &str) -> Option<(bool, String)> {
    let user = std::env::var("USER").ok()?;
    let subuid = std::fs::read_to_string("/etc/subuid").ok()?;
    if !subuid.lines().any(|l| l.split(':').next() == Some(user.as_str()))
        || !["podman", "setpriv", "timeout"].iter().all(|b| on_path(b))
    {
        return None;
    }
    let a: SocketAddr = direct.parse().ok()?;
    let script = format!("exec 3<>/dev/tcp/{}/{}", a.ip(), a.port());
    let out = tokio::process::Command::new("timeout")
        .args([
            "10",
            "podman",
            "unshare",
            "setpriv",
            "--reuid=1",
            "--regid=1",
            "--clear-groups",
            "--",
            "bash",
            "-c",
            &script,
        ])
        .output()
        .await
        .ok()?;
    Some(if out.status.success() {
        (false, format!("a process under a subordinate uid reached {a} directly: the firewall doesn't cover them"))
    } else {
        (true, format!("a process under a subordinate uid can't reach {a}"))
    })
}
