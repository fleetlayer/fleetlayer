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
        for bad in ["", "a/b", "a.b", "a b", &"x".repeat(33)] {
            assert!(!valid_instance(bad), "{bad:?}");
        }
    }
}
