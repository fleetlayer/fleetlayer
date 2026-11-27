//! `fleetlayer me`: what a session may use. The node tells the control plane which tools it shares (`link/tools`:
//! names, descriptions, folder names, never paths), when it links and whenever an opt-in changes; the control plane
//! answers `link/me` for one of this node's sessions from that and the access policy, without asking anyone.

use std::path::Path;

use fleetlayer_plugin::Peer;
use fleetlayer_protocol::link::{self as l, FolderInfo, ToolInfo};

use crate::Node;
use crate::tools::ToolConfig;

/// What this node shares, as `link/tools` carries it.
pub async fn infos(node: &Node) -> Vec<ToolInfo> {
    let mut out = vec![];
    for t in &node.tools {
        out.push(ToolInfo {
            name: t.name.clone(),
            description: t.description.clone(),
            allow: t.allow.clone(),
            not_offered: crate::tools::network_refusal(node, t).await,
            folders: folders(t),
        });
    }
    out
}

/// The folders of a `fleetlayer-folders` tool, from its arguments: names and grants, never their paths.
pub fn folders(t: &ToolConfig) -> Vec<FolderInfo> {
    if Path::new(&t.command).file_name().is_none_or(|n| n != "fleetlayer-folders") {
        return vec![];
    }
    let mut out: Vec<FolderInfo> = vec![];
    let mut args = t.args.iter();
    let mut grants: Vec<(String, String)> = vec![];
    while let Some(a) = args.next() {
        let (flag, value) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || value.clone().or_else(|| args.next().cloned());
        match flag {
            "--folder" => {
                if let Some(v) = value()
                    && let Some((name, _path)) = v.split_once('=')
                {
                    out.push(FolderInfo { name: name.to_string(), ..Default::default() });
                }
            }
            "--upload" | "--delete" | "--trusted" => {
                if let Some(v) = value() {
                    grants.push((flag.to_string(), v));
                }
            }
            _ => {}
        }
    }
    for (flag, name) in grants {
        if let Some(f) = out.iter_mut().find(|f| f.name == name) {
            match flag.as_str() {
                "--upload" => f.upload = true,
                "--delete" => f.delete = true,
                _ => f.trusted = true,
            }
        }
    }
    out
}

/// Tells the control plane what this node shares, if it's linked (else it's sent when it links).
pub async fn announce(node: &Node) {
    if let Some(peer) = node.link.peer().await {
        announce_to(node, &peer).await;
    }
}

pub async fn announce_to(node: &Node, peer: &Peer) {
    peer.notify(l::method::TOOLS, &l::Tools { tools: infos(node).await });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(command: &str, args: &[&str]) -> ToolConfig {
        serde_json::from_value(serde_json::json!({
            "name": "folders", "command": command, "args": args,
        }))
        .unwrap()
    }

    #[test]
    fn folders_are_named_with_their_grants_and_never_their_paths() {
        let t = tool(
            "/usr/local/bin/fleetlayer-folders",
            &[
                "--folder",
                "screenshots=~/Screenshots",
                "--folder=downloads=/Users/x/Downloads",
                "--upload",
                "downloads",
                "--delete=downloads",
                "--trusted",
                "screenshots",
                "--upload",
                "nonesuch",
            ],
        );
        let f = folders(&t);
        assert_eq!(
            f,
            vec![
                FolderInfo { name: "screenshots".into(), trusted: true, ..Default::default() },
                FolderInfo { name: "downloads".into(), upload: true, delete: true, trusted: false },
            ]
        );
        assert!(!serde_json::to_string(&f).unwrap().contains("Users"));
    }

    #[test]
    fn other_tools_have_no_folders() {
        assert!(folders(&tool("node", &["--folder", "a=b"])).is_empty());
    }
}
