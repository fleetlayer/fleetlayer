//! Where a slot's credentials are stored: a file on Linux, the login keychain on macOS. Both hold the same JSON Claude
//! Code writes itself (`{"claudeAiOauth":{…}}`).
//!
//! On macOS the keychain is reached through `/usr/bin/security`, behind the `Security` trait so the rest is tested on
//! Linux. A write goes through `security -i` on stdin, with the value as hex (`-X`), so the token never appears in a
//! process list. `security -i` exits 0 even when a command fails, so every write is read back and compared.
//! Writing needs the login keychain unlocked: on macOS the node runs as a per-user LaunchAgent in a logged-in session.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::claude::{credentials_file, keychain_item};

pub trait Store: Send + Sync {
    /// The slot's stored credentials, or None when there are none.
    fn read(&self, slot: &str) -> anyhow::Result<Option<String>>;
    fn write(&self, slot: &str, json: &str) -> anyhow::Result<()>;
    /// Remove the slot's credentials; fine if there are none.
    fn remove(&self, slot: &str) -> anyhow::Result<()>;
}

/// Linux: `<slot>/.credentials.json`, mode 0600, written atomically.
pub struct FileStore {
    pub home: String,
}

impl FileStore {
    fn file(&self, slot: &str) -> anyhow::Result<String> {
        credentials_file(slot, Some(&self.home)).ok_or_else(|| anyhow::anyhow!("no home for the default slot"))
    }
}

pub fn write_0600(path: &Path, content: &[u8]) -> std::io::Result<()> {
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

impl Store for FileStore {
    fn read(&self, slot: &str) -> anyhow::Result<Option<String>> {
        match std::fs::read_to_string(self.file(slot)?) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, slot: &str, json: &str) -> anyhow::Result<()> {
        Ok(write_0600(Path::new(&self.file(slot)?), json.as_bytes())?)
    }

    fn remove(&self, slot: &str) -> anyhow::Result<()> {
        match std::fs::remove_file(self.file(slot)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// What `security` printed and its exit code.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `/usr/bin/security`.
pub trait Security: Send + Sync {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<Ran>;
}

pub struct RealSecurity;

impl Security for RealSecurity {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<Ran> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/security")
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            pipe.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        Ok(Ran {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// `security`'s exit code for an item that isn't there (errSecItemNotFound).
const NOT_FOUND: i32 = 44;

/// macOS: the login keychain, as Claude Code keeps it.
pub struct KeychainStore {
    /// The keychain account (`claude::keychain_user`).
    pub user: String,
    pub security: Box<dyn Security>,
}

impl Store for KeychainStore {
    fn read(&self, slot: &str) -> anyhow::Result<Option<String>> {
        let item = keychain_item(slot);
        let r = self.security.run(&["find-generic-password", "-a", &self.user, "-w", "-s", &item], None)?;
        match r.code {
            0 => Ok(Some(r.stdout.trim_end_matches('\n').to_string())),
            NOT_FOUND => Ok(None),
            code => anyhow::bail!("reading {item}: security exited {code}: {}", r.stderr.trim()),
        }
    }

    fn write(&self, slot: &str, json: &str) -> anyhow::Result<()> {
        let item = keychain_item(slot);
        // `security -i` splits its line like a shell: the item name has a space, so it's quoted. The account is a plain
        // name (keychain_user) and the value is hex, so neither needs quoting.
        let line = format!("add-generic-password -U -a {} -s \"{item}\" -X {}\n", self.user, hex::encode(json));
        let r = self.security.run(&["-i"], Some(&line))?;
        match self.read(slot)? {
            Some(stored) if stored == json => Ok(()),
            _ => anyhow::bail!("writing {item} didn't stick (is the login keychain locked?): {}", r.stderr.trim()),
        }
    }

    fn remove(&self, slot: &str) -> anyhow::Result<()> {
        let item = keychain_item(slot);
        let r = self.security.run(&["delete-generic-password", "-a", &self.user, "-s", &item], None)?;
        match r.code {
            0 | NOT_FOUND => Ok(()),
            code => anyhow::bail!("removing {item}: security exited {code}: {}", r.stderr.trim()),
        }
    }
}

/// The store Claude Code uses on this OS.
pub fn native(home: &str, user: Option<&str>) -> Box<dyn Store> {
    if cfg!(target_os = "macos") {
        Box::new(KeychainStore { user: crate::claude::keychain_user(user), security: Box::new(RealSecurity) })
    } else {
        Box::new(FileStore { home: home.into() })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    /// A keychain in memory that answers the commands `KeychainStore` sends, the way `security` does.
    #[derive(Default)]
    pub struct FakeSecurity {
        pub items: Mutex<BTreeMap<(String, String), String>>,
        pub calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
        pub locked: bool,
    }

    fn arg<'a>(args: &'a [&str], flag: &str) -> &'a str {
        args.iter().position(|a| *a == flag).map(|i| args[i + 1]).unwrap_or_default()
    }

    impl Security for FakeSecurity {
        fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<Ran> {
            self.calls.lock().unwrap().push((args.iter().map(|s| s.to_string()).collect(), stdin.map(Into::into)));
            let mut items = self.items.lock().unwrap();
            let ran = |code| Ok(Ran { code, ..Default::default() });
            match args.first().copied() {
                Some("-i") => {
                    let line = stdin.unwrap_or_default().trim_end();
                    // add-generic-password -U -a <user> -s "<item>" -X <hex>
                    let (head, hexed) = line.rsplit_once(" -X ").unwrap();
                    let (head, item) = head.split_once(" -s ").unwrap();
                    let user = head.rsplit(' ').next().unwrap();
                    let item = item.trim_matches('"');
                    if !self.locked {
                        let value = String::from_utf8(hex::decode(hexed).unwrap()).unwrap();
                        items.insert((user.into(), item.into()), value);
                    }
                    ran(0) // as `security -i` does, even when the command failed
                }
                Some("find-generic-password") => match items.get(&(arg(args, "-a").into(), arg(args, "-s").into())) {
                    Some(v) => Ok(Ran { code: 0, stdout: format!("{v}\n"), stderr: String::new() }),
                    None => ran(NOT_FOUND),
                },
                Some("delete-generic-password") => {
                    match items.remove(&(arg(args, "-a").into(), arg(args, "-s").into())) {
                        Some(_) => ran(0),
                        None => ran(NOT_FOUND),
                    }
                }
                _ => ran(1),
            }
        }
    }

    pub fn keychain(locked: bool) -> (KeychainStore, std::sync::Arc<FakeSecurity>) {
        let fake = std::sync::Arc::new(FakeSecurity { locked, ..Default::default() });
        struct Shared(std::sync::Arc<FakeSecurity>);
        impl Security for Shared {
            fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<Ran> {
                self.0.run(args, stdin)
            }
        }
        (KeychainStore { user: "tester".into(), security: Box::new(Shared(fake.clone())) }, fake)
    }

    #[test]
    fn keychain_writes_go_through_stdin_as_hex_and_are_read_back() {
        let (store, fake) = keychain(false);
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-secret"}}"#;
        store.write("/Users/tester/.claude/accounts/2", json).unwrap();
        let calls = fake.calls.lock().unwrap().clone();
        let (args, stdin) = &calls[0];
        assert_eq!(args, &vec!["-i".to_string()]);
        let stdin = stdin.clone().unwrap();
        assert!(stdin.contains("-s \"Claude Code-credentials-a296c876\""), "{stdin}");
        assert!(!stdin.contains("sk-ant"), "the token is sent as hex only");
        // Nothing with the token ever goes on a command line.
        assert!(calls.iter().all(|(a, _)| a.iter().all(|x| !x.contains("sk-ant"))));
        assert_eq!(store.read("/Users/tester/.claude/accounts/2").unwrap().as_deref(), Some(json));
        assert_eq!(store.read("/Users/tester/.claude/accounts/3").unwrap(), None);
        store.remove("/Users/tester/.claude/accounts/2").unwrap();
        store.remove("/Users/tester/.claude/accounts/2").unwrap();
        assert_eq!(store.read("/Users/tester/.claude/accounts/2").unwrap(), None);
    }

    #[test]
    fn the_default_slot_is_the_unsuffixed_item() {
        let (store, fake) = keychain(false);
        store.write("", "{}").unwrap();
        assert!(fake.items.lock().unwrap().contains_key(&("tester".into(), "Claude Code-credentials".into())));
    }

    #[test]
    fn a_write_that_didnt_stick_is_an_error() {
        let (store, _) = keychain(true);
        let e = store.write("/s/2", "{}").unwrap_err().to_string();
        assert!(e.contains("locked"), "{e}");
    }

    #[test]
    fn file_store_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let store = FileStore { home: d.path().display().to_string() };
        let slot = d.path().join("2");
        std::fs::create_dir_all(&slot).unwrap();
        let slot = slot.display().to_string();
        assert_eq!(store.read(&slot).unwrap(), None);
        store.write(&slot, "{\"a\":1}").unwrap();
        assert_eq!(store.read(&slot).unwrap().as_deref(), Some("{\"a\":1}"));
        let mode = std::fs::metadata(format!("{slot}/.credentials.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        store.remove(&slot).unwrap();
        store.remove(&slot).unwrap();
        assert_eq!(store.read(&slot).unwrap(), None);
    }

    /// Against the real keychain: CI's macOS job runs it with a throwaway keychain as the default
    /// (`cargo test -- --ignored real_keychain`). Never run it on a laptop with a real login keychain: it writes an item
    /// for a slot path nobody uses and removes it, but it does write.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn real_keychain_round_trip() {
        let user = crate::claude::keychain_user(std::env::var("USER").ok().as_deref());
        let store = KeychainStore { user: user.clone(), security: Box::new(RealSecurity) };
        let slot = format!("/tmp/fleetlayer-keychain-test-{}", std::process::id());
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","expiresAt":1,"scopes":["user:inference"]}}"#;
        store.write(&slot, json).unwrap();
        assert_eq!(store.read(&slot).unwrap().as_deref(), Some(json));
        // Read as Claude Code reads it: by item name and account, the value as the plain JSON.
        let r =
            RealSecurity.run(&["find-generic-password", "-a", &user, "-w", "-s", &keychain_item(&slot)], None).unwrap();
        assert_eq!((r.code, r.stdout.trim()), (0, json));
        // Overwriting updates the same item (-U).
        store.write(&slot, "{\"claudeAiOauth\":{}}").unwrap();
        assert_eq!(store.read(&slot).unwrap().as_deref(), Some("{\"claudeAiOauth\":{}}"));
        store.remove(&slot).unwrap();
        assert_eq!(store.read(&slot).unwrap(), None);
    }
}
