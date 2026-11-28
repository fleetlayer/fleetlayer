//! Reading cookies from the person's running, signed-in Chrome through Playwright MCP's extension connection (the
//! Playwright MCP Bridge extension), the way Fleetlayer's shared browser tool reaches it: no cookie file, no Keychain,
//! no privacy grant. Its `browser_cookie_list` tool (behind `--caps=storage`) returns HttpOnly cookies too.
//!
//! The Playwright MCP process here is this plugin's own: started for one read, with `--caps=storage`, and stopped right
//! after, its one blank tab closed. It's never shared or offered as a tool. The agent-facing browser tool must never
//! have storage capabilities, or every agent allowed the browser could read every cookie (the node refuses to share a
//! tool configured with them).
//!
//! `browser_cookie_list` answers `name=value (domain: d, path: p)` lines: no expiry or flags, so a cookie read this
//! way has `expires: None`. Playwright reads the profile's cookies into its own process on this machine and filters
//! them there; only the share's domains (and names) reach this plugin.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::chrome::{Cookie, in_domains};

/// How to start Playwright MCP: the same install and extension token as the shared browser tool
/// (`docs/guides/browser.md`), with the defaults that guide uses.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Playwright {
    /// The `node` program (default `/opt/homebrew/bin/node` when it's there, else `node` on the PATH).
    #[serde(default)]
    pub node: Option<PathBuf>,
    /// Playwright MCP's `cli.js` (default `~/.local/share/browser-mcp/node_modules/@playwright/mcp/cli.js`).
    #[serde(default)]
    pub cli: Option<PathBuf>,
    /// The Playwright MCP Bridge extension's token (default `~/.config/browser-mcp/extension-token`).
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Chrome's executable (default the usual one on macOS).
    #[serde(default)]
    pub chrome: Option<PathBuf>,
}

impl Playwright {
    /// The settings with `~` expanded and the defaults filled in.
    pub fn resolved(&self, home: &Path) -> Playwright {
        let exp = |p: &Path| crate::expand(home, p);
        Playwright {
            node: Some(self.node.as_deref().map(exp).unwrap_or_else(|| {
                let brew = PathBuf::from("/opt/homebrew/bin/node");
                if brew.exists() { brew } else { PathBuf::from("node") }
            })),
            cli: Some(
                self.cli
                    .as_deref()
                    .map(exp)
                    .unwrap_or_else(|| home.join(".local/share/browser-mcp/node_modules/@playwright/mcp/cli.js")),
            ),
            token_file: Some(
                self.token_file.as_deref().map(exp).unwrap_or_else(|| home.join(".config/browser-mcp/extension-token")),
            ),
            chrome: self.chrome.as_deref().map(exp).or_else(|| {
                cfg!(target_os = "macos")
                    .then(|| PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"))
            }),
        }
    }
}

/// The cookies in a `browser_cookie_list` answer. Lines that aren't cookies (headings, the code it ran) are skipped.
pub fn parse_list(text: &str) -> Vec<Cookie> {
    let mut out = vec![];
    for line in text.lines() {
        let Some(rest) = line.strip_suffix(')') else { continue };
        let Some((pair, attrs)) = rest.rsplit_once(" (domain: ") else { continue };
        let Some((domain, path)) = attrs.rsplit_once(", path: ") else { continue };
        let Some((name, value)) = pair.split_once('=') else { continue };
        if name.is_empty() || domain.is_empty() {
            continue;
        }
        out.push(Cookie {
            domain: domain.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            path: path.to_string(),
            expires: None,
            secure: false,
            http_only: false,
            same_site: None,
        });
    }
    out
}

/// One MCP conversation with a Playwright MCP child over stdio.
struct Mcp {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next: i64,
}

impl Mcp {
    async fn send(&mut self, method: &str, params: Value, notify: bool) -> Result<Value, String> {
        let mut msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if !notify {
            self.next += 1;
            msg["id"] = json!(self.next);
        }
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("talking to Playwright MCP: {e}"))?;
        if notify {
            return Ok(Value::Null);
        }
        loop {
            let line = self
                .stdout
                .next_line()
                .await
                .map_err(|e| format!("reading Playwright MCP: {e}"))?
                .ok_or("Playwright MCP stopped (is the Playwright MCP Bridge extension installed in Chrome?)")?;
            let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
            if m["id"] != json!(self.next) {
                continue;
            }
            if let Some(e) = m.get("error") {
                return Err(format!("Playwright MCP: {}", e["message"].as_str().unwrap_or("an error")));
            }
            return Ok(m["result"].clone());
        }
    }

    /// A tool's text answer; a tool error is an Err with its text.
    async fn call(&mut self, tool: &str, args: Value) -> Result<String, String> {
        let r = self.send("tools/call", json!({"name": tool, "arguments": args}), false).await?;
        let text: Vec<&str> =
            r["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
        let text = text.join("\n");
        if r["isError"] == true {
            // The text of an error names what went wrong, never cookie values (none were listed).
            return Err(format!(
                "{tool}: {}",
                text.lines().find(|l| !l.trim().is_empty() && !l.starts_with('#')).unwrap_or("failed")
            ));
        }
        Ok(text)
    }
}

/// The cookies of `domains` (each with its subdomains) in the signed-in Chrome's `profile`, through a Playwright MCP
/// process started for this read and stopped after it.
pub async fn read(pw: &Playwright, profile: &str, domains: &[String]) -> Result<Vec<Cookie>, String> {
    let token_file = pw.token_file.clone().unwrap_or_default();
    let token = std::fs::read_to_string(&token_file)
        .map_err(|e| format!("reading the extension token {}: {e} (docs/guides/browser.md)", token_file.display()))?;
    let cli = pw.cli.clone().unwrap_or_default();
    if !cli.exists() {
        return Err(format!("Playwright MCP isn't installed at {} (docs/guides/browser.md)", cli.display()));
    }
    let cwd = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new(pw.node.clone().unwrap_or_else(|| PathBuf::from("node")));
    cmd.arg(&cli)
        .args(["--extension", "--caps=storage", "--profile-dir-name", profile, "--session-name", "fleetlayer cookies"])
        .arg("--ignore-roots")
        .current_dir(cwd.path())
        .env("PLAYWRIGHT_MCP_EXTENSION_TOKEN", token.trim())
        .env_remove("PLAYWRIGHT_MCP_CAPS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(chrome) = &pw.chrome {
        cmd.env("PLAYWRIGHT_MCP_EXECUTABLE_PATH", chrome);
    }
    let mut child = cmd.spawn().map_err(|e| format!("starting Playwright MCP: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let mut mcp = Mcp { child, stdin, stdout, next: 0 };
    let result = tokio::time::timeout(Duration::from_secs(90), list(&mut mcp, domains))
        .await
        .unwrap_or_else(|_| Err("Chrome didn't answer within 90 seconds (is it running, with the extension?)".into()));
    // Close the one blank tab this connection opened, then stop the process.
    let _ =
        tokio::time::timeout(Duration::from_secs(10), mcp.call("browser_tabs", json!({"action": "close", "index": 0})))
            .await;
    drop(mcp.stdin);
    if tokio::time::timeout(Duration::from_secs(10), mcp.child.wait()).await.is_err() {
        let _ = mcp.child.kill().await;
    }
    result
}

async fn list(mcp: &mut Mcp, domains: &[String]) -> Result<Vec<Cookie>, String> {
    mcp.send(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "fleetlayer-cookies", "version": env!("CARGO_PKG_VERSION")}}),
        false,
    )
    .await?;
    mcp.send("notifications/initialized", json!({}), true).await?;
    let tools = mcp.send("tools/list", json!({}), false).await?;
    let offered = tools["tools"].as_array().into_iter().flatten().any(|t| t["name"] == "browser_cookie_list");
    if !offered {
        return Err("this Playwright MCP has no browser_cookie_list (it needs 0.0.86 or later)".into());
    }
    let mut out: Vec<Cookie> = vec![];
    for d in domains {
        let text = mcp.call("browser_cookie_list", json!({"domain": d})).await?;
        for c in parse_list(&text) {
            if in_domains(&c.domain, domains)
                && !out.iter().any(|o| o.domain == c.domain && o.name == c.name && o.path == c.path)
            {
                out.push(c);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_list_answer_parses_and_skips_everything_else() {
        let text = "### Result\nauth_token=abc123 (domain: .x.com, path: /)\nct0=a=b (c) (domain: .x.com, path: /)\n\
                    lang=en (domain: upload.x.com, path: /i)\n### Ran Playwright code\n```js\nawait page.context().cookies();\n```";
        let got = parse_list(text);
        let got: Vec<_> =
            got.iter().map(|c| (c.name.as_str(), c.value.as_str(), c.domain.as_str(), c.path.as_str())).collect();
        assert_eq!(
            got,
            [
                ("auth_token", "abc123", ".x.com", "/"),
                ("ct0", "a=b (c)", ".x.com", "/"),
                ("lang", "en", "upload.x.com", "/i")
            ]
        );
        assert!(parse_list("### Result\nNo cookies found").is_empty());
    }

    #[test]
    fn defaults_follow_the_browser_guide() {
        let pw = Playwright::default().resolved(Path::new("/home/a"));
        assert_eq!(pw.cli.unwrap(), Path::new("/home/a/.local/share/browser-mcp/node_modules/@playwright/mcp/cli.js"));
        assert_eq!(pw.token_file.unwrap(), Path::new("/home/a/.config/browser-mcp/extension-token"));
    }

    /// A stand-in for Playwright MCP that checks how it's started and answers like it.
    #[tokio::test]
    async fn a_read_starts_its_own_playwright_with_storage_lists_the_domains_and_closes_its_tab() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("log");
        let fake = d.path().join("cli.js");
        let script = include_str!("../tests/fake-playwright-mcp.sh").replace("LOG", &log.display().to_string());
        std::fs::write(&fake, script).unwrap();
        let token = d.path().join("token");
        std::fs::write(&token, "tok\n").unwrap();
        let pw = Playwright { node: Some("/bin/sh".into()), cli: Some(fake), token_file: Some(token), chrome: None };
        let got = read(&pw, "Default", &["x.com".into()]).await.unwrap();
        assert_eq!(
            got.iter().map(|c| (c.name.as_str(), c.value.as_str())).collect::<Vec<_>>(),
            [("auth_token", "abc")]
        );
        let log = std::fs::read_to_string(&log).unwrap();
        assert!(log.contains("--extension --caps=storage --profile-dir-name Default"), "{log}");
        assert!(log.contains("token: tok\n"), "{log}");
        assert!(log.contains(r#""arguments":{"domain":"x.com"}"#), "{log}");
        assert!(log.contains(r#""arguments":{"action":"close","index":0}"#), "its tab is closed: {log}");
    }
}
