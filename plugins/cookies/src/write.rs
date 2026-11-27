//! Writing a set's cookies on the target in the format a tool reads: a Netscape `cookies.txt` (curl, wget, yt-dlp,
//! gallery-dl), a JSON file in Playwright's `storageState` shape, or an env file of named values (`AUTH_TOKEN=…`).
//! Each file is written mode 0600 and replaced in one rename.

use std::collections::BTreeMap;

use fleetlayer_secrets::write::env_value;
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

pub use fleetlayer_secrets::write::atomic;

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
}
