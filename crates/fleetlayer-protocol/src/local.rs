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

#[cfg(test)]
mod tests {
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
}
