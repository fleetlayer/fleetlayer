//! Where a machine's own Fleetlayer pieces find each other, shared so the node, the CLI, the gateway and plugins
//! agree.

use std::path::PathBuf;

/// `$FLEETLAYER_INSTANCE`: a name for a second Fleetlayer under the same account (a test copy beside the real one).
/// It gives the user services their own names (`dev.fleetlayer.<instance>.node`) and the node its own socket under
/// `$XDG_RUNTIME_DIR`; give the copy its own `HOME` too, for its files. Unset (the usual case), nothing changes.
pub fn instance() -> Option<String> {
    let v = std::env::var("FLEETLAYER_INSTANCE").ok()?;
    let ok = !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then_some(v)
}

/// The node's socket when nothing says otherwise: under `$XDG_RUNTIME_DIR` when there is one, else
/// `~/.local/state/fleetlayer/node.sock`.
pub fn default_node_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => {
            let dir = match instance() {
                Some(i) => format!("fleetlayer-{i}"),
                None => "fleetlayer".into(),
            };
            PathBuf::from(d).join(dir).join("node.sock")
        }
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/fleetlayer/node.sock"),
    }
}
