//! Where a machine's own Fleetlayer pieces find each other, shared so the node, the CLI, the gateway and plugins
//! agree.

use std::path::{Path, PathBuf};

/// `$FLEETLAYER_INSTANCE`: a name for a second Fleetlayer under the same account (a test copy beside the real one).
/// It gives the user services their own names (`dev.fleetlayer.<instance>.node`) and the node its own socket under
/// `$XDG_RUNTIME_DIR`; give the copy its own `HOME` too, for its files. Unset (the usual case), nothing changes.
pub fn instance() -> Option<String> {
    std::env::var("FLEETLAYER_INSTANCE").ok().filter(|v| valid_instance(v))
}

fn valid_instance(v: &str) -> bool {
    !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The node's socket when nothing says otherwise: under `$XDG_RUNTIME_DIR` when there is one, else
/// `~/.local/state/fleetlayer/node.sock`.
pub fn default_node_socket() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    node_socket_in(std::env::var_os("XDG_RUNTIME_DIR").as_deref().map(Path::new), &home, instance().as_deref())
}

pub fn node_socket_in(runtime_dir: Option<&Path>, home: &Path, instance: Option<&str>) -> PathBuf {
    match (runtime_dir, instance) {
        (Some(d), None) => d.join("fleetlayer/node.sock"),
        (Some(d), Some(i)) => d.join(format!("fleetlayer-{i}/node.sock")),
        (None, _) => home.join(".local/state/fleetlayer/node.sock"),
    }
}

/// A user service's launchd label: `dev.fleetlayer.node`, or `dev.fleetlayer.<instance>.node`.
pub fn launchd_label(name: &str, instance: Option<&str>) -> String {
    match instance {
        Some(i) => format!("dev.fleetlayer.{i}.{name}"),
        None => format!("dev.fleetlayer.{name}"),
    }
}

/// A user service's systemd unit: `fleetlayer-node.service`, or `fleetlayer-<instance>-node.service`.
pub fn systemd_unit(name: &str, instance: Option<&str>) -> String {
    match instance {
        Some(i) => format!("fleetlayer-{i}-{name}.service"),
        None => format!("fleetlayer-{name}.service"),
    }
}

/// The PATH written into a user service, from the installing shell's: launchd and systemd don't expand `~` or `$HOME`
/// and run services in another folder, so a leading `~` and `$HOME` are expanded against the account's own home (where
/// the shell found its tools, even for a second instance with another `HOME`), relative and empty entries (the current
/// folder) are dropped, and so are repeats (the first one stays).
pub fn service_path(path: &str, account_home: &Path) -> String {
    let home = account_home.display().to_string();
    let mut out: Vec<String> = vec![];
    for e in path.split(':') {
        let e = if e == "~" || e == "$HOME" || e == "${HOME}" {
            home.clone()
        } else if let Some(rest) = e.strip_prefix("~/").or(e.strip_prefix("$HOME/")).or(e.strip_prefix("${HOME}/")) {
            format!("{}/{rest}", home.trim_end_matches('/'))
        } else {
            e.to_string()
        };
        if e.starts_with('/') && !out.contains(&e) {
            out.push(e);
        }
    }
    out.join(":")
}

/// A program as the service will find it: the first `dir/name` on `path` (as [`service_path`] writes it) that is a file.
pub fn find_on(path: &str, name: &str) -> Option<PathBuf> {
    path.split(':').map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

/// Replace `path` with `text` atomically: a temporary file beside it, created with the mode it ends up with (the
/// file's own, else `default_mode`), so the text is never readable more widely even for a moment; then renamed over
/// it.
pub fn write_atomic(path: &Path, text: &str, default_mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(default_mode);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    // The umask may have narrowed it; never widen beyond `mode`.
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Where Fleetlayer's Chrome opener lives: `~/.local/state/fleetlayer/open-chrome`.
pub fn open_chrome_path(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/open-chrome")
}

/// A stand-in for Chrome's executable, for Playwright MCP's extension mode on macOS (`PLAYWRIGHT_MCP_EXECUTABLE_PATH`).
/// Playwright MCP starts Chrome's own binary with its connect page's address; started by a Fleetlayer process, that
/// Chrome counts as Fleetlayer's, and macOS shows "would like to access data from other apps" at every connection.
/// This passes only the address (the last argument) to the Chrome that's running, through Launch Services, so macOS
/// checks nothing. Chrome's other arguments (`--profile-directory`) are dropped: the page opens in the profile last
/// used.
pub const OPEN_CHROME_SCRIPT: &str = r#"#!/bin/sh
# Written by Fleetlayer (fleetlayer_protocol::local::OPEN_CHROME_SCRIPT): opens the address Playwright MCP passes in
# the Chrome that's running, so macOS doesn't ask about access to other apps' data at every connection.
last=
for a in "$@"; do last=$a; done
case $last in
  -*|'') exec /usr/bin/open -g -b com.google.Chrome ;;
  *) exec /usr/bin/open -g -b com.google.Chrome "$last" ;;
esac
"#;

/// Write the Chrome opener at `path` (mode 0755) unless it's already there with this content.
pub fn ensure_open_chrome(path: &Path) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|s| s == OPEN_CHROME_SCRIPT) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, OPEN_CHROME_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {

    #[test]
    fn an_atomic_write_keeps_the_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("fl-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("node.yaml");
        write_atomic(&f, "a: 1\n", 0o600).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&f, "a: 2\n", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a: 2\n");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temporary file left");
        std::fs::remove_dir_all(&d).unwrap();
    }
    use super::*;

    /// Nathan's nodes and every existing install rely on these: with no instance they must never change.
    #[test]
    fn defaults_without_an_instance_are_pinned() {
        let home = Path::new("/home/a");
        assert_eq!(
            node_socket_in(Some(Path::new("/run/user/1000")), home, None),
            Path::new("/run/user/1000/fleetlayer/node.sock")
        );
        assert_eq!(node_socket_in(None, home, None), Path::new("/home/a/.local/state/fleetlayer/node.sock"));
        for name in ["node", "control", "gateway"] {
            assert_eq!(launchd_label(name, None), format!("dev.fleetlayer.{name}"));
            assert_eq!(systemd_unit(name, None), format!("fleetlayer-{name}.service"));
        }
        // Absolute entries stay as they are, in order; only empty ones go.
        assert_eq!(service_path("/usr/bin::/home/a/.local/bin:", home), "/usr/bin:/home/a/.local/bin");
    }

    #[test]
    fn service_path_expands_home_and_drops_what_a_service_cant_use() {
        let acct = Path::new("/Users/a");
        assert_eq!(
            service_path(
                "/b:~/.local/bin:./node_modules/.bin:$HOME/.dotnet/tools:~/.local/bin:/usr/bin:${HOME}/x:bin::/b:~",
                acct
            ),
            "/b:/Users/a/.local/bin:/Users/a/.dotnet/tools:/usr/bin:/Users/a/x:/Users/a"
        );
        assert_eq!(service_path("~/.local/bin", Path::new("/")), "/.local/bin");
    }

    #[test]
    fn an_instance_gets_its_own_names() {
        let home = Path::new("/tmp/try");
        assert_eq!(
            node_socket_in(Some(Path::new("/run/user/1000")), home, Some("try")),
            Path::new("/run/user/1000/fleetlayer-try/node.sock")
        );
        assert_eq!(node_socket_in(None, home, Some("try")), Path::new("/tmp/try/.local/state/fleetlayer/node.sock"));
        assert_eq!(launchd_label("node", Some("try")), "dev.fleetlayer.try.node");
        assert_eq!(systemd_unit("gateway", Some("try")), "fleetlayer-try-gateway.service");
        // An instance's HOME doesn't change what `~` meant to the shell: the account's own home, where its tools are.
        assert_eq!(service_path("/b:~/.local/bin", Path::new("/Users/a")), "/b:/Users/a/.local/bin");
        for bad in ["", "a/b", "a.b", "a b", &"x".repeat(33)] {
            assert!(!valid_instance(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_chrome_opener_passes_only_the_last_address() {
        let dir = std::env::temp_dir().join(format!("fleetlayer-open-chrome-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("open-chrome");
        // The same script with `open` swapped for printing what it would run.
        std::fs::write(&script, OPEN_CHROME_SCRIPT.replace("exec /usr/bin/open", "echo open")).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("/bin/sh").arg(&script).args(args).output().unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_eq!(
            run(&["--profile-directory=Default", "--no-first-run", "chrome-extension://abc/connect.html?token=1"]),
            "open -g -b com.google.Chrome chrome-extension://abc/connect.html?token=1"
        );
        assert_eq!(run(&["--no-first-run"]), "open -g -b com.google.Chrome", "no address: just bring Chrome up");
        let path = open_chrome_path(&dir);
        ensure_open_chrome(&path).unwrap();
        ensure_open_chrome(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OPEN_CHROME_SCRIPT);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
