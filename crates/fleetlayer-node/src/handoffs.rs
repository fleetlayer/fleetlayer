//! Handoffs (`fleetlayer session handoff`): move a session's work to a new session, usually on another node. This
//! node writes the new session's first prompt (the handoff's ID, the note or where to read it, and how to read the old
//! transcript); the control plane keeps the note, checks the policy, starts the session and links old and new.

use std::hash::{BuildHasher, Hasher};

use fleetlayer_protocol::link::{self as l, HandoffStarted, SessionHandoff, SessionStart, StartSpec};
use fleetlayer_protocol::rpc::{RpcError, code};

use crate::Node;
use crate::sessions::Session;

/// A note this long or shorter goes into the first prompt as it is; a longer one is read with
/// `fleetlayer session handoff-note`.
const NOTE_INLINE_MAX: usize = 2000;

/// What `node/session_handoff` asks for.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ask {
    /// The session whose work moves: `<tab>@<node>`, a tab here, or the caller itself when absent.
    pub from: Option<String>,
    /// The node the new session starts on.
    pub to: String,
    pub note: Option<String>,
    /// The new session's tab: the old one's when absent.
    pub tab: Option<String>,
    /// Where the new session works: the old one's folder when absent and it's on this node.
    pub cwd: Option<String>,
    pub harness: Option<String>,
    /// More for the new session to do, after reading the handoff.
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub why: Option<String>,
}

/// Hand `ask.from`'s work to a new session on `ask.to`, for `caller`.
pub async fn start(node: &Node, ask: Ask, caller: Option<&Session>) -> Result<HandoffStarted, RpcError> {
    let (from, local) = match &ask.from {
        None => {
            let c = caller.ok_or_else(|| {
                RpcError::invalid_params("say which session hands off: a shell isn't a session (<tab>@<node>)")
            })?;
            (address(node, c), Some(c.clone()))
        }
        Some(f) => match f.rsplit_once('@') {
            Some((_, n)) if n != node.name => (f.clone(), None),
            Some((name, _)) => {
                let s = crate::deliver::resolve(node, name).await?;
                (address(node, &s), Some(s))
            }
            None => {
                let s = crate::deliver::resolve(node, f).await?;
                (address(node, &s), Some(s))
            }
        },
    };
    let tab = ask.tab.clone().unwrap_or_else(|| from.rsplit_once('@').map(|(t, _)| t).unwrap_or(&from).to_string());
    let cwd = ask.cwd.clone().or_else(|| local.as_ref().and_then(|s| s.cwd.clone())).ok_or_else(|| {
        RpcError::invalid_params(format!("say where the new session works (--cwd): {from} isn't on this node"))
    })?;
    let id = new_id();
    let text = prompt(&id, &from, ask.note.as_deref(), ask.prompt.as_deref());
    let (sender, reply_to) = crate::start::sender(node, caller);
    let spec = StartSpec {
        tab: Some(tab),
        cwd: Some(cwd),
        harness: ask.harness,
        prompt: Some(crate::deliver::render_message(&sender, &text, reply_to.as_deref())),
        resume: None,
        model: ask.model,
        effort: ask.effort,
    };
    let req = SessionHandoff {
        id,
        from,
        note: ask.note,
        start: SessionStart {
            node: ask.to,
            spec,
            caller: caller.and_then(Session::tab).unwrap_or("-").to_string(),
            justification: ask.why,
        },
    };
    let peer = node.link.peer().await.ok_or_else(|| {
        RpcError::not_here(format!("{} isn't linked to a control plane now, and handoffs go through it", node.name))
    })?;
    peer.request_within(l::method::SESSION_HANDOFF, &req, crate::start::REMOTE_TIMEOUT).await.map_err(|e| {
        if e.code == code::METHOD_NOT_FOUND {
            RpcError::refused(format!(
                "the control plane is older than this node and can't hand sessions off: update it to Fleetlayer {} \
                 or later",
                env!("CARGO_PKG_VERSION")
            ))
        } else {
            e
        }
    })
}

/// One handoff, with its note, for `caller` (the session it went to, or one that may read where it came from).
pub async fn get(node: &Node, id: &str, caller: Option<&Session>, why: Option<String>) -> Result<l::Handoff, RpcError> {
    let peer = node
        .link
        .peer()
        .await
        .ok_or_else(|| RpcError::not_here(format!("{} isn't linked to a control plane now", node.name)))?;
    let req = l::HandoffGet {
        id: id.to_string(),
        caller: caller.and_then(Session::tab).unwrap_or("-").to_string(),
        justification: why,
    };
    peer.request(l::method::HANDOFF_GET, &req).await
}

fn address(node: &Node, s: &Session) -> String {
    format!("{}@{}", s.tab().unwrap_or(&s.session_id), node.name)
}

/// `h` and 16 hex digits, from the process's random hasher seed and the time.
fn new_id() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    h.write_u128(now.as_nanos());
    h.write_u32(std::process::id());
    format!("h{:016x}", h.finish())
}

/// The new session's first prompt.
pub fn prompt(id: &str, from: &str, note: Option<&str>, more: Option<&str>) -> String {
    let mut out = format!("You're taking over the work of {from} (Fleetlayer handoff {id}).");
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if n.len() <= NOTE_INLINE_MAX => {
            out.push_str(&format!(
                "\n\nIts handoff note:\n\n{n}\n\n(Read it again with `fleetlayer session handoff-note {id}`.)"
            ));
        }
        Some(n) => out.push_str(&format!(
            "\n\nRead its handoff note ({} KB) first: `fleetlayer session handoff-note {id}`.",
            n.len().div_ceil(1000)
        )),
        None => {}
    }
    out.push_str(&format!(
        "\n\nTo see where it got to, read its transcript: `fleetlayer transcript {from} --tail 20000000 --types \
         user,assistant` (earlier turns: add `--before <the offset it prints>`)."
    ));
    if let Some(m) = more.map(str::trim).filter(|m| !m.is_empty()) {
        out.push_str(&format!("\n\n{m}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_note_goes_in_the_prompt_and_a_long_one_is_pointed_to() {
        let p = prompt("h0123456789abcdef", "api@laptop", Some("Tests pass; deploy is next."), Some("Then deploy."));
        assert!(p.starts_with("You're taking over the work of api@laptop (Fleetlayer handoff h0123456789abcdef)."));
        assert!(p.contains("Its handoff note:\n\nTests pass; deploy is next."), "{p}");
        assert!(p.contains("fleetlayer transcript api@laptop --tail 20000000 --types user,assistant"), "{p}");
        assert!(p.ends_with("\n\nThen deploy."), "{p}");
        let p = prompt("h0123456789abcdef", "api@laptop", Some(&"x".repeat(5000)), None);
        assert!(p.contains("Read its handoff note (5 KB) first: `fleetlayer session handoff-note h0123456789abcdef`"));
        assert!(!p.contains("xxxx"));
    }

    #[test]
    fn ids_are_h_and_sixteen_hex_digits_and_differ() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 17);
        assert!(a.starts_with('h') && a[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
