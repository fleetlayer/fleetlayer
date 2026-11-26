//! Delivering a message to a session (`docs/protocol.md` section 5). The node owns this, not the plugins: it resolves
//! the address, renders the text, refuses repeats, and picks the plugin. A harness plugin that can reach the agent
//! through its own channel goes first; the session manager typing it in is the fallback, and only after a definite
//! "not delivered", never after a timeout. Nothing is queued or retried: the sender learns at once.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fleetlayer_protocol::ext::{self, Deliver, Delivered, DeliveryKind, From};
use fleetlayer_protocol::rpc::{RpcError, code};
use tokio::sync::Mutex;

use crate::Node;
use crate::sessions::Session;

const SEEN_LIMIT: usize = 512;
/// A delivery is cut off after this; the plugin's own tools time out sooner.
const DELIVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Deliveries {
    seen: Mutex<VecDeque<String>>,
    counter: AtomicU64,
}

impl Deliveries {
    pub fn new_id(&self, node: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{node}-{}-{n}", jiff::Timestamp::now().as_millisecond())
    }

    /// False when this ID was delivered already.
    async fn first_time(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().await;
        if seen.iter().any(|s| s == id) {
            return false;
        }
        if seen.len() == SEEN_LIMIT {
            seen.pop_front();
        }
        seen.push_back(id.into());
        true
    }
}

/// `<tab>@<node>`, `<tab>` or a session ID, split into the name and the node it names (None: this node).
pub fn parse_address(to: &str) -> (&str, Option<&str>) {
    match to.rsplit_once('@') {
        // `@w1-3` is an unnamed session's ID, not a node.
        Some((name, node)) if !name.is_empty() => (name, Some(node)),
        _ => (to, None),
    }
}

/// The session on this node that `name` names.
pub async fn resolve(node: &Node, name: &str) -> Result<Session, RpcError> {
    let mut found = node.registry.find(name).await;
    match found.len() {
        0 => Err(RpcError::not_here(format!("no running session called {name} on {}", node.name))),
        1 => {
            let s = found.remove(0);
            if s.running() { Ok(s) } else { Err(RpcError::not_here(format!("{name} on {} isn't running", node.name))) }
        }
        _ => {
            let ids: Vec<String> = found.iter().map(|s| s.session_id.clone()).collect();
            Err(RpcError::refused(format!(
                "{name} is ambiguous on {}: {} sessions have that name ({}); send to a session ID instead",
                node.name,
                ids.len(),
                ids.join(", ")
            )))
        }
    }
}

/// The text a session sees for a message from another session or a plugin.
pub fn render_message(from: &From, text: &str, reply_to: Option<&str>) -> String {
    let mut out = format!("Message from {} via Fleetlayer:\n\n{text}", from.label);
    if let Some(to) = reply_to {
        out.push_str(&format!("\n\nReply with: fleetlayer msg send --to \"{to}\" \"…\""));
    }
    out
}

/// Send `text` to the session `to` names, from `from`. `caller` is the sending session, when it's one of this
/// node's: replies go to it.
pub async fn send(
    node: &Node,
    to: &str,
    text: &str,
    from: From,
    caller: Option<&Session>,
    why: Option<String>,
) -> Result<Delivered, RpcError> {
    let (name, target_node) = parse_address(to);
    let reply_to = caller.and_then(|c| c.tab().map(|t| format!("{t}@{}", node.name)));
    let rendered = render_message(&from, text, reply_to.as_deref());
    if let Some(n) = target_node
        && n != node.name
    {
        let peer = node.link.peer().await.ok_or_else(|| {
            RpcError::not_here(format!(
                "{to} is on another node ({n}), and this node isn't linked to a control plane now"
            ))
        })?;
        let req = fleetlayer_protocol::link::SendRequest {
            to: to.into(),
            delivery_id: node.deliveries.new_id(&node.name),
            from,
            text: rendered,
            justification: why,
        };
        return peer.request_within(fleetlayer_protocol::link::method::SEND, &req, Duration::from_secs(12)).await;
    }
    let session = resolve(node, name).await?;
    let d = Deliver {
        session_id: session.session_id.clone(),
        address: session.meta.address.clone().unwrap_or_default(),
        delivery_id: node.deliveries.new_id(&node.name),
        kind: DeliveryKind::Message,
        subkind: None,
        untrusted: false,
        from,
        text: rendered,
    };
    deliver(node, &session, &d).await
}

/// Put a rendered delivery in front of a session.
pub async fn deliver(node: &Node, session: &Session, d: &Deliver) -> Result<Delivered, RpcError> {
    if !node.deliveries.first_time(&d.delivery_id).await {
        return Err(RpcError::refused(format!("delivery {} was already delivered", d.delivery_id)));
    }
    if let Some(h) = &session.harness_plugin {
        match try_plugin(node, h, d).await {
            Err(e) if e.code == code::NO_CHANNEL || e.code == code::NOT_HERE => {
                tracing::debug!(session = %session.session_id, "harness {h} couldn't deliver ({e}); typing it in");
            }
            other => return other,
        }
    }
    if !node.typing_fallback {
        return Err(RpcError::not_here(format!(
            "session {} has no messaging channel now, and typing into its terminal is off on {} (typing_fallback)",
            session.session_id, node.name
        )));
    }
    let Some(m) = &session.manager else {
        return Err(RpcError::not_here(format!("no plugin runs session {}", session.session_id)));
    };
    try_plugin(node, m, d).await
}

async fn try_plugin(node: &Node, plugin: &str, d: &Deliver) -> Result<Delivered, RpcError> {
    let p = node
        .plugin(plugin)
        .ok_or_else(|| RpcError::new(code::NO_CHANNEL, format!("plugin {plugin} isn't configured")))?;
    let peer =
        p.peer().await.ok_or_else(|| RpcError::new(code::NO_CHANNEL, format!("plugin {plugin} isn't running")))?;
    peer.request_within(ext::method::SESSION_DELIVER, d, DELIVER_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_split_into_name_and_node() {
        assert_eq!(parse_address("planning@laptop"), ("planning", Some("laptop")));
        assert_eq!(parse_address("planning"), ("planning", None));
        assert_eq!(parse_address("@w1-3"), ("@w1-3", None), "an unnamed session's ID");
        assert_eq!(parse_address("a@b@laptop"), ("a@b", Some("laptop")));
    }

    #[test]
    fn a_message_names_its_sender_and_how_to_reply() {
        let from = From { label: "ci (laptop)".into(), ..Default::default() };
        assert_eq!(
            render_message(&from, "green", Some("ci@laptop")),
            "Message from ci (laptop) via Fleetlayer:\n\ngreen\n\nReply with: fleetlayer msg send --to \"ci@laptop\" \"…\""
        );
        assert_eq!(render_message(&from, "x", None), "Message from ci (laptop) via Fleetlayer:\n\nx");
    }

    #[tokio::test]
    async fn a_delivery_id_is_accepted_once() {
        let d = Deliveries::default();
        assert!(d.first_time("a").await);
        assert!(!d.first_time("a").await);
        assert_ne!(d.new_id("n"), d.new_id("n"));
    }
}
