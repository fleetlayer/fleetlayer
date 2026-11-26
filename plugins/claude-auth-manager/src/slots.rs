//! Writing setup tokens into account slots, the way Claude Code reads them (tested November 2026, Claude Code
//! 2.1.332): `{"claudeAiOauth":{"accessToken":…,"expiresAt":…,"scopes":["user:inference"],"subscriptionType":"max"}}`
//! with no refresh token, in `<slot>/.credentials.json` (mode 0600) on Linux and the login keychain on macOS
//! (`store.rs`). Claude Code uses it as it is and leaves it alone.
//!
//! A slot this plugin wrote has a `.fleetlayer-managed` marker file in its directory (on macOS too, where the
//! credentials themselves are in the keychain). Applying a bundle writes every
//! account's slot and removes the credentials of managed slots that left the bundle; a slot without the marker that
//! already has credentials (someone signed into it by hand) is never touched.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::{Store, write_0600};

pub const MARKER: &str = ".fleetlayer-managed";

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

    /// The slot as Claude Code names it: the empty string for the default sign-in, the directory for any other.
    fn key(&self, slot: u32) -> String {
        if slot == 1 { String::new() } else { slot_path(&self.pattern, slot).display().to_string() }
    }

    /// The slot named by its directory.
    fn key_of(&self, dir: &Path) -> String {
        if dir == self.default_dir { String::new() } else { dir.display().to_string() }
    }

    /// The slots this plugin manages now (they have its marker), with what the marker says.
    pub fn managed(&self) -> Vec<Managed> {
        let mut dirs = vec![self.default_dir.clone()];
        if let Some(parent) = slot_path(&self.pattern, 0).parent()
            && let Ok(rd) = std::fs::read_dir(parent)
        {
            let mut found: Vec<PathBuf> =
                rd.flatten().map(|e| e.path()).filter(|d| matches_pattern(&self.pattern, d)).collect();
            found.sort();
            dirs.extend(found);
        }
        dirs.into_iter()
            .filter_map(|dir| {
                let text = std::fs::read_to_string(dir.join(MARKER)).ok()?;
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                Some(Managed {
                    slot: self.key_of(&dir),
                    account_uuid: v["accountUuid"].as_str().unwrap_or_default().into(),
                    email: v["email"].as_str().unwrap_or_default().into(),
                })
            })
            .collect()
    }
}

/// A slot this plugin wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Managed {
    /// As Claude Code names it (`Layout::key`).
    pub slot: String,
    pub account_uuid: String,
    pub email: String,
}

fn slot_path(pattern: &str, slot: u32) -> PathBuf {
    PathBuf::from(pattern.replace("{slot}", &slot.to_string()))
}

/// Make the managed slots match `accounts`.
pub fn apply(layout: &Layout, store: &dyn Store, accounts: &[Account]) -> anyhow::Result<Applied> {
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
        let key = layout.key(a.slot);
        let marker = dir.join(MARKER);
        if !marker.exists() && store.read(&key)?.is_some() {
            out.skipped.push(format!("slot {}: signed into by hand (no {MARKER}), left alone", a.slot));
            continue;
        }
        let entry = serde_json::json!({"claudeAiOauth": {
            "accessToken": a.token,
            "expiresAt": expires.as_millisecond(),
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        }});
        store.write(&key, &serde_json::to_string(&entry)?)?;
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
        store.remove("")?;
        let _ = std::fs::remove_file(layout.default_dir.join(MARKER));
        out.removed += 1;
    }
    if let Some(parent) = slot_path(pattern, 0).parent()
        && let Ok(rd) = std::fs::read_dir(parent)
    {
        for e in rd.flatten() {
            let dir = e.path();
            if dir.join(MARKER).exists() && !wanted.contains(&dir) && matches_pattern(pattern, &dir) {
                store.remove(&layout.key_of(&dir))?;
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
    use crate::store::FileStore;

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
        Layout { pattern: format!("{}/slot-{{slot}}", d.display()), default_dir: d.join(".claude"), default_slot }
    }

    fn files(d: &Path) -> FileStore {
        FileStore { home: d.display().to_string() }
    }

    #[test]
    fn the_default_slot_is_written_only_when_asked_and_never_over_a_hand_sign_in() {
        let d = tempfile::tempdir().unwrap();
        let r = apply(&layout(d.path(), false), &files(d.path()), &[acct(1), acct(2)]).unwrap();
        assert_eq!((r.written, r.skipped.len()), (1, 1));
        assert!(!d.path().join(".claude/.credentials.json").exists());
        let r = apply(&layout(d.path(), true), &files(d.path()), &[acct(1), acct(2)]).unwrap();
        assert_eq!(r.written, 2);
        assert!(d.path().join(".claude/.credentials.json").exists());
        // Turned off again: the managed default slot is cleared.
        let r = apply(&layout(d.path(), false), &files(d.path()), &[acct(2)]).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!d.path().join(".claude/.credentials.json").exists());
        // Someone signs in by hand there: never overwritten.
        std::fs::write(d.path().join(".claude/.credentials.json"), "hand").unwrap();
        let r = apply(&layout(d.path(), true), &files(d.path()), &[acct(1)]).unwrap();
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(d.path().join(".claude/.credentials.json")).unwrap(), "hand");
    }

    #[test]
    fn slots_match_the_bundle_and_hand_signed_slots_are_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let pattern = layout(d.path(), false);
        // Slot 3 was signed into by hand.
        std::fs::create_dir_all(d.path().join("slot-3")).unwrap();
        std::fs::write(d.path().join("slot-3/.credentials.json"), "{\"hand\":true}").unwrap();
        let r = apply(&pattern, &files(d.path()), &[acct(4), acct(2), acct(3)]).unwrap();
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
        let r = apply(&pattern, &files(d.path()), &[acct(4)]).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!d.path().join("slot-2/.credentials.json").exists());
        assert!(d.path().join("slot-3/.credentials.json").exists());
        assert!(apply(&pattern, &files(d.path()), &[Account { token: "nope".into(), ..acct(5) }]).is_err());
    }

    #[test]
    fn slot_directories_are_recognised_by_the_pattern() {
        assert!(matches_pattern("/x/slot-{slot}", Path::new("/x/slot-12")));
        assert!(!matches_pattern("/x/slot-{slot}", Path::new("/x/slot-abc")));
        assert!(!matches_pattern("/x/slot-{slot}", Path::new("/x/other")));
        assert!(matches_pattern("/x/{slot}", Path::new("/x/5")));
    }

    #[test]
    fn on_macos_the_tokens_go_to_the_keychain_and_markers_to_the_slot_directories() {
        let d = tempfile::tempdir().unwrap();
        let (store, fake) = crate::store::tests::keychain(false);
        let l = layout(d.path(), true);
        // The person's own sign-in in the default item: never touched.
        fake.items.lock().unwrap().insert(("tester".into(), "Claude Code-credentials".into()), "hand".into());
        let r = apply(&l, &store, &[acct(1), acct(2)]).unwrap();
        assert_eq!((r.written, r.skipped.len()), (1, 1));
        let item = crate::claude::keychain_item(&d.path().join("slot-2").display().to_string());
        let stored = fake.items.lock().unwrap().get(&("tester".into(), item.clone())).cloned().unwrap();
        assert!(stored.contains("sk-ant-oat01-test2"));
        assert!(!d.path().join("slot-2/.credentials.json").exists(), "no file on macOS");
        assert!(d.path().join("slot-2/.fleetlayer-managed").exists());
        assert_eq!(l.managed().iter().map(|m| m.account_uuid.as_str()).collect::<Vec<_>>(), vec!["uuid-2"]);
        // Account 2 leaves: its item goes, the hand sign-in stays.
        let r = apply(&l, &store, &[acct(1)]).unwrap();
        assert_eq!(r.removed, 1);
        let items = fake.items.lock().unwrap();
        assert!(!items.contains_key(&("tester".into(), item)));
        assert_eq!(items.get(&("tester".into(), "Claude Code-credentials".into())).map(String::as_str), Some("hand"));
    }

    #[test]
    fn managed_lists_the_marked_slots_with_their_accounts() {
        let d = tempfile::tempdir().unwrap();
        let l = layout(d.path(), true);
        apply(&l, &files(d.path()), &[acct(1), acct(3), acct(2)]).unwrap();
        let m = l.managed();
        assert_eq!(m.iter().map(|m| m.account_uuid.as_str()).collect::<Vec<_>>(), vec!["uuid-1", "uuid-2", "uuid-3"]);
        assert_eq!(m[0].slot, "", "the default slot is named by the empty string");
        assert_eq!(m[1].slot, d.path().join("slot-2").display().to_string());
    }
}
