//! Writing setup tokens into account slots, the way Claude Code reads them (tested November 2026, Claude Code
//! 2.1.332): on Linux `<slot>/.credentials.json`, mode 0600, holding
//! `{"claudeAiOauth":{"accessToken":…,"expiresAt":…,"scopes":["user:inference"],"subscriptionType":"max"}}` with no
//! refresh token. Claude Code uses it as it is and leaves it alone. (macOS keeps it in the login keychain instead; that
//! writer is the claude rotation session's part.)
//!
//! A slot this plugin wrote has a `.fleetlayer-managed` marker beside its credentials. Applying a bundle writes every
//! account's slot and removes the credentials of managed slots that left the bundle; a slot without the marker that
//! already has credentials (someone signed into it by hand) is never touched.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MARKER: &str = ".fleetlayer-managed";
const CREDENTIALS: &str = ".credentials.json";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub slot: u32,
    pub account_uuid: String,
    pub email: String,
    /// When the token stops working (a year after `claude setup-token` made it), RFC 3339.
    pub expires_at: String,
    pub token: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub written: usize,
    pub removed: usize,
    /// Slots left alone, with why.
    pub skipped: Vec<String>,
}

/// Where the slots are: `pattern` for accounts 2 and up (the rotator's `~/.claude/accounts/{slot}`), and account 1's
/// default slot (Claude Code's normal sign-in, `~/.claude`), written only when `default_slot` is on.
#[derive(Debug, Clone)]
pub struct Layout {
    pub pattern: String,
    pub default_dir: PathBuf,
    pub default_slot: bool,
}

impl Layout {
    fn path(&self, slot: u32) -> PathBuf {
        if slot == 1 { self.default_dir.clone() } else { slot_path(&self.pattern, slot) }
    }
}

fn slot_path(pattern: &str, slot: u32) -> PathBuf {
    PathBuf::from(pattern.replace("{slot}", &slot.to_string()))
}

fn write_0600(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("fleetlayer-tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// Make the managed slots match `accounts`.
pub fn apply(layout: &Layout, accounts: &[Account]) -> anyhow::Result<Applied> {
    let pattern = layout.pattern.as_str();
    let mut out = Applied::default();
    for a in accounts {
        anyhow::ensure!(a.slot >= 1, "slots start at 1");
        if a.slot == 1 && !layout.default_slot {
            out.skipped.push("slot 1: the default sign-in slot, written only with default_slot: true".into());
            continue;
        }
        anyhow::ensure!(a.token.starts_with("sk-ant-"), "slot {}: that isn't a Claude token", a.slot);
        let expires: jiff::Timestamp =
            a.expires_at.parse().map_err(|e| anyhow::anyhow!("slot {}: expiresAt: {e}", a.slot))?;
        let dir = layout.path(a.slot);
        std::fs::create_dir_all(&dir)?;
        if a.slot != 1 {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let creds = dir.join(CREDENTIALS);
        let marker = dir.join(MARKER);
        if creds.exists() && !marker.exists() {
            out.skipped.push(format!("slot {}: signed into by hand (no {MARKER}), left alone", a.slot));
            continue;
        }
        let entry = serde_json::json!({"claudeAiOauth": {
            "accessToken": a.token,
            "expiresAt": expires.as_millisecond(),
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        }});
        write_0600(&creds, serde_json::to_string(&entry)?.as_bytes())?;
        write_0600(
            &marker,
            serde_json::json!({"accountUuid": a.account_uuid, "email": a.email}).to_string().as_bytes(),
        )?;
        out.written += 1;
    }
    // Managed slots no longer in the bundle: find them next to the slots we know (the pattern's parent).
    let wanted: Vec<PathBuf> = accounts.iter().map(|a| layout.path(a.slot)).collect();
    // The default slot, when this plugin wrote it and account 1 left the bundle (or default_slot was turned off).
    let default_wanted = layout.default_slot && accounts.iter().any(|a| a.slot == 1);
    if !default_wanted && layout.default_dir.join(MARKER).exists() {
        let _ = std::fs::remove_file(layout.default_dir.join(CREDENTIALS));
        let _ = std::fs::remove_file(layout.default_dir.join(MARKER));
        out.removed += 1;
    }
    if let Some(parent) = slot_path(pattern, 0).parent()
        && let Ok(rd) = std::fs::read_dir(parent)
    {
        for e in rd.flatten() {
            let dir = e.path();
            if dir.join(MARKER).exists() && !wanted.contains(&dir) && matches_pattern(pattern, &dir) {
                let _ = std::fs::remove_file(dir.join(CREDENTIALS));
                let _ = std::fs::remove_file(dir.join(MARKER));
                out.removed += 1;
            }
        }
    }
    Ok(out)
}

/// Whether `dir` is a slot directory under `pattern` (its name fits the pattern with a number for `{slot}`).
fn matches_pattern(pattern: &str, dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else { return false };
    let Some(last) = Path::new(pattern).file_name().and_then(|n| n.to_str()) else { return false };
    let Some((pre, post)) = last.split_once("{slot}") else { return false };
    name.strip_prefix(pre)
        .and_then(|r| r.strip_suffix(post))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(slot: u32) -> Account {
        Account {
            slot,
            account_uuid: format!("uuid-{slot}"),
            email: format!("a{slot}@example.com"),
            expires_at: "2027-11-26T00:00:00Z".into(),
            token: format!("sk-ant-oat01-test{slot}"),
        }
    }

    fn layout(d: &Path, default_slot: bool) -> Layout {
        Layout { pattern: format!("{}/slot-{{slot}}", d.display()), default_dir: d.join("default"), default_slot }
    }

    #[test]
    fn the_default_slot_is_written_only_when_asked_and_never_over_a_hand_sign_in() {
        let d = tempfile::tempdir().unwrap();
        let r = apply(&layout(d.path(), false), &[acct(1), acct(2)]).unwrap();
        assert_eq!((r.written, r.skipped.len()), (1, 1));
        assert!(!d.path().join("default/.credentials.json").exists());
        let r = apply(&layout(d.path(), true), &[acct(1), acct(2)]).unwrap();
        assert_eq!(r.written, 2);
        assert!(d.path().join("default/.credentials.json").exists());
        // Turned off again: the managed default slot is cleared.
        let r = apply(&layout(d.path(), false), &[acct(2)]).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!d.path().join("default/.credentials.json").exists());
        // Someone signs in by hand there: never overwritten.
        std::fs::write(d.path().join("default/.credentials.json"), "hand").unwrap();
        let r = apply(&layout(d.path(), true), &[acct(1)]).unwrap();
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(d.path().join("default/.credentials.json")).unwrap(), "hand");
    }

    #[test]
    fn slots_match_the_bundle_and_hand_signed_slots_are_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let pattern = layout(d.path(), false);
        // Slot 3 was signed into by hand.
        std::fs::create_dir_all(d.path().join("slot-3")).unwrap();
        std::fs::write(d.path().join("slot-3/.credentials.json"), "{\"hand\":true}").unwrap();
        let r = apply(&pattern, &[acct(4), acct(2), acct(3)]).unwrap();
        assert_eq!((r.written, r.removed, r.skipped.len()), (2, 0, 1));
        let creds = std::fs::read_to_string(d.path().join("slot-4/.credentials.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&creds).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "sk-ant-oat01-test4");
        assert_eq!(v["claudeAiOauth"]["scopes"][0], "user:inference");
        assert!(v["claudeAiOauth"].get("refreshToken").is_none());
        let mode = std::fs::metadata(d.path().join("slot-4/.credentials.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(d.path().join("slot-3/.credentials.json")).unwrap(), "{\"hand\":true}");
        // Account 2 leaves the bundle: its managed slot is cleared, slot 3 still untouched.
        let r = apply(&pattern, &[acct(4)]).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!d.path().join("slot-2/.credentials.json").exists());
        assert!(d.path().join("slot-3/.credentials.json").exists());
        assert!(apply(&pattern, &[Account { token: "nope".into(), ..acct(5) }]).is_err());
    }

    #[test]
    fn slot_directories_are_recognised_by_the_pattern() {
        assert!(matches_pattern("/x/slot-{slot}", Path::new("/x/slot-12")));
        assert!(!matches_pattern("/x/slot-{slot}", Path::new("/x/slot-abc")));
        assert!(!matches_pattern("/x/slot-{slot}", Path::new("/x/other")));
        assert!(matches_pattern("/x/{slot}", Path::new("/x/5")));
    }
}
