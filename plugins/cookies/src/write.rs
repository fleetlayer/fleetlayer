//! Writing a set's cookies on the target in the format a tool reads: a Netscape `cookies.txt` (curl, wget, yt-dlp,
//! gallery-dl), a JSON file in Playwright's `storageState` shape, or an env file of named values (`AUTH_TOKEN=…`).
//! Each file is written mode 0600 and replaced in one rename.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::chrome::Cookie;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Netscape,
    Json,
    Env,
}

/// The cookie named `name` to use for a named value: when several domains of the set have one, the one that expires
/// last (a session cookie counts as last).
pub fn pick<'a>(cookies: &'a [Cookie], name: &str) -> Option<&'a Cookie> {
    cookies.iter().filter(|c| c.name == name).max_by_key(|c| c.expires.unwrap_or(i64::MAX))
}

pub fn netscape(cookies: &[Cookie]) -> String {
    let mut out = String::from(
        "# Netscape HTTP Cookie File\n# Written by fleetlayer-cookies, and replaced whenever the cookies change.\n\n",
    );
    for c in cookies {
        let sub = if c.host_only() { "FALSE" } else { "TRUE" };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        let line = [
            c.domain.as_str(),
            sub,
            c.path.as_str(),
            secure,
            &c.expires.unwrap_or(0).to_string(),
            c.name.as_str(),
            c.value.as_str(),
        ]
        .join("\t");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Playwright's `storageState` (`browser.newContext({storageState})`), which other tools read too.
pub fn json(cookies: &[Cookie]) -> String {
    let list: Vec<_> = cookies
        .iter()
        .map(|c| {
            json!({
                "name": c.name, "value": c.value, "domain": c.domain, "path": c.path,
                "expires": c.expires.unwrap_or(-1), "httpOnly": c.http_only, "secure": c.secure,
                "sameSite": c.same_site.as_deref().unwrap_or("Lax"),
            })
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&json!({"cookies": list, "origins": []})).unwrap_or_default();
    s.push('\n');
    s
}

/// A value as a shell and dotenv readers take it: bare when it's plain, else in single quotes.
fn env_value(v: &str) -> String {
    let plain = !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "_-.%:/+=~@,".contains(c));
    if plain { v.to_string() } else { format!("'{}'", v.replace('\'', r"'\''")) }
}

/// `VAR=value` lines, one per entry of `values` (variable → cookie name); an error names the cookies that are missing.
pub fn env(cookies: &[Cookie], values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::from("# Written by fleetlayer-cookies, and replaced whenever the cookies change.\n");
    let mut missing = vec![];
    for (var, name) in values {
        match pick(cookies, name) {
            Some(c) if c.value.contains(['\n', '\r', '\0']) => return Err(format!("the cookie {name} isn't one line")),
            Some(c) => out.push_str(&format!("{var}={}\n", env_value(&c.value))),
            None => missing.push(name.as_str()),
        }
    }
    if missing.is_empty() { Ok(out) } else { Err(format!("no cookie named {} in what was sent", missing.join(", "))) }
}

/// Write `content` to `path`, mode 0600, through a file beside it and one rename. A missing folder is made, mode
/// 0700.
pub fn atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = path.parent().ok_or_else(|| std::io::Error::other("a path with no folder"))?;
    if !parent.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let tmp = parent.join(format!(
        ".{}.fleetlayer-tmp",
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(domain: &str, name: &str, value: &str, expires: Option<i64>) -> Cookie {
        Cookie {
            domain: domain.into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            expires,
            secure: true,
            http_only: true,
            same_site: None,
        }
    }

    #[test]
    fn netscape_lines() {
        let s = netscape(&[c(".x.com", "ct0", "v", Some(1_900_000_000)), c("api.x.com", "s", "w", None)]);
        assert!(s.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(s.contains(".x.com\tTRUE\t/\tTRUE\t1900000000\tct0\tv\n"));
        assert!(s.contains("api.x.com\tFALSE\t/\tTRUE\t0\ts\tw\n"));
    }

    #[test]
    fn json_is_a_storage_state() {
        let v: serde_json::Value = serde_json::from_str(&json(&[c(".x.com", "ct0", "v", None)])).unwrap();
        assert_eq!(v["cookies"][0]["expires"], -1);
        assert_eq!(v["cookies"][0]["sameSite"], "Lax");
        assert_eq!(v["origins"], serde_json::json!([]));
    }

    #[test]
    fn env_names_values_and_quotes_what_needs_it() {
        let cookies = [
            c(".x.com", "auth_token", "abc123", Some(10)),
            c(".twitter.com", "auth_token", "newer", Some(20)),
            c(".x.com", "ct0", "it's \"odd\"", Some(10)),
        ];
        let values: BTreeMap<_, _> =
            [("AUTH_TOKEN".to_string(), "auth_token".to_string()), ("CT0".into(), "ct0".into())].into();
        let s = env(&cookies, &values).unwrap();
        assert!(s.contains("AUTH_TOKEN=newer\n"), "the one that expires last: {s}");
        assert!(s.contains(r#"CT0='it'\''s "odd"'"#), "{s}");
        let missing: BTreeMap<_, _> = [("X".to_string(), "nope".to_string())].into();
        assert_eq!(env(&cookies, &missing).unwrap_err(), "no cookie named nope in what was sent");
    }

    #[test]
    fn atomic_writes_are_private_and_replace_links_rather_than_follow_them() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new/dir/cookies.txt");
        atomic(&p, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let elsewhere = d.path().join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let link = d.path().join("link.txt");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        atomic(&link, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "two");
    }
}
