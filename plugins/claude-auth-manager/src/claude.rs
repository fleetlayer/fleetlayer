//! Where Claude Code keeps an account slot's sign-in, as Claude Code 2.1.332 does it (the same rules the rotator's
//! `hooks/model.ts` mirrors). A slot is a directory the rotator puts in `CLAUDE_SECURESTORAGE_CONFIG_DIR`; the empty
//! slot is Claude Code's default sign-in, with that variable unset.
//!
//! - macOS: the login keychain item `Claude Code-credentials`, suffixed for any other slot with `-` and the first 8
//!   hex digits of the SHA-256 of the slot's path, NFC-normalized; account `$USER`.
//! - Linux: `<slot>/.credentials.json`, or `~/.claude/.credentials.json` for the default slot.

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The keychain item Claude Code keeps a slot's sign-in in.
pub fn keychain_item(slot: &str) -> String {
    if slot.is_empty() {
        return "Claude Code-credentials".into();
    }
    let nfc: String = slot.nfc().collect();
    format!("Claude Code-credentials-{}", &sha256_hex(&nfc)[..8])
}

/// The file Claude Code keeps a slot's sign-in in where there is no keychain (Linux). None for the default slot with
/// no home.
pub fn credentials_file(slot: &str, home: Option<&str>) -> Option<String> {
    let dir = if slot.is_empty() { format!("{}/.claude", home.filter(|h| !h.is_empty())?) } else { slot.to_string() };
    let nfc: String = dir.nfc().collect();
    Some(format!("{nfc}/.credentials.json"))
}

/// The keychain account Claude Code files its items under: `$USER` when it's a plain name.
pub fn keychain_user(user: Option<&str>) -> String {
    match user {
        Some(u) if !u.is_empty() && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) => {
            u.into()
        }
        _ => "claude-code-user".into(),
    }
}

/// Variables that make Claude Code use some other credential than the slot's (an API key, a fixed OAuth token, another
/// provider), so a child meant to use a slot runs without them.
pub const OVERRIDING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_items_match_claude_code() {
        assert_eq!(keychain_item(""), "Claude Code-credentials");
        // Computed with Python's hashlib for the same paths.
        assert_eq!(keychain_item("/Users/tester/.claude/accounts/2"), "Claude Code-credentials-a296c876");
        // A decomposed é (as macOS file APIs can hand it over) hashes as the composed one.
        assert_eq!(keychain_item("/home/Jose\u{301}/.claude/accounts/3"), "Claude Code-credentials-69fab773");
        assert_eq!(keychain_item("/home/Jos\u{e9}/.claude/accounts/3"), "Claude Code-credentials-69fab773");
    }

    #[test]
    fn credentials_files() {
        assert_eq!(credentials_file("", Some("/home/a")).as_deref(), Some("/home/a/.claude/.credentials.json"));
        assert_eq!(credentials_file("", None), None);
        assert_eq!(credentials_file("/s/2", None).as_deref(), Some("/s/2/.credentials.json"));
    }

    #[test]
    fn keychain_users_are_plain_names() {
        assert_eq!(keychain_user(Some("alice")), "alice");
        assert_eq!(keychain_user(Some("a b")), "claude-code-user");
        assert_eq!(keychain_user(None), "claude-code-user");
    }
}
