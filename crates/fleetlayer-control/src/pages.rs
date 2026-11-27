//! `fleetlayer pages push` from machines without the pages vault: which node hosts each pages space (from
//! `link/pages_spaces`, kept in memory), and opening a push to it (`link/pages_open`), checked as `write` on
//! `page:<space>/<name>` and recorded like any other stream.

use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::link::{self as l, PagesOpen, PagesServe, ToolOpened};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::Value;

use crate::App;

/// A space or page name: lower-case letters, digits and hyphens.
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 60
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `node` hosts these spaces now, replacing what it said before. A space another node hosts while it's linked stays
/// with that node, loudly: two services writing one space would each show half of it. Returns the spaces refused.
pub async fn claim(app: &App, tenant: &str, node: &str, spaces: &[String]) -> Vec<String> {
    let online = app.links.online(tenant).await;
    let mut map = app.links.pages.lock().await;
    map.retain(|(t, space), host| !(t == tenant && host == node && !spaces.contains(space)));
    let mut refused = vec![];
    for space in spaces {
        if !valid_name(space) {
            tracing::warn!(node, space, "ignoring a pages space with an unusable name");
            refused.push(space.clone());
            continue;
        }
        let key = (tenant.to_string(), space.clone());
        match map.get(&key) {
            Some(other) if other != node && online.contains(other) => {
                tracing::error!(
                    tenant,
                    space,
                    host = %other,
                    claimed_by = node,
                    "two nodes host the pages space {space}: {other} keeps it and {node}'s is ignored; give one of \
                     them another name"
                );
                refused.push(space.clone());
            }
            Some(other) if other != node => {
                tracing::warn!(tenant, space, from = %other, to = node, "the pages space moved to another node");
                map.insert(key, node.to_string());
            }
            _ => {
                map.insert(key, node.to_string());
            }
        }
    }
    refused
}

/// A push from one of `node`'s sessions: checked, then a stream to the space's node.
pub async fn open(app: &Arc<App>, tenant: &str, node: &str, t: PagesOpen) -> Result<ToolOpened, RpcError> {
    if !valid_name(&t.space) {
        return Err(RpcError::invalid_params(format!(
            "space {:?}: use lower-case letters, digits and hyphens",
            t.space
        )));
    }
    if !valid_name(&t.name) {
        return Err(RpcError::invalid_params(format!(
            "page name {:?}: use lower-case letters, digits and hyphens",
            t.name
        )));
    }
    if t.date.as_deref().is_some_and(|d| d.len() != 10 || !d.bytes().all(|b| b.is_ascii_digit() || b == b'-')) {
        return Err(RpcError::invalid_params("date: use YYYY-MM-DD"));
    }
    let caller = format!("{}@{}", t.caller, node);
    let target = format!("page:{}/{}", t.space, t.name);
    crate::authz::check_why(app, tenant, &caller, "write", &target, t.justification.as_deref()).await?;
    let host = app
        .links
        .pages
        .lock()
        .await
        .get(&(tenant.to_string(), t.space.clone()))
        .cloned()
        .ok_or_else(|| RpcError::not_here(format!("no pages service has a space called {}", t.space)))?;
    let server = app
        .links
        .peer(tenant, &host)
        .await
        .ok_or_else(|| RpcError::not_here(format!("the pages node for {} ({host}) is offline", t.space)))?;
    let id = app
        .streams
        .create(&app.db, tenant, &format!("pages:{}/{}", t.space, t.name), node, &host, &caller)
        .await
        .map_err(RpcError::internal)?;
    // A stream nobody opens is forgotten.
    let (app2, id2) = (app.clone(), id.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        app2.streams.forget(&id2).await;
    });
    let serve = PagesServe {
        stream_id: id.clone(),
        space: t.space.clone(),
        name: t.name.clone(),
        new: t.new,
        date: t.date.clone(),
        caller,
    };
    if let Err(e) = server.request_within::<_, Value>(l::method::PAGES_SERVE, &serve, Duration::from_secs(20)).await {
        app.streams.forget(&id).await;
        crate::streams::finish(app, &id, 0, 0, &format!("refused: {}", e.message)).await;
        return Err(e);
    }
    Ok(ToolOpened { stream_id: id })
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use fleetlayer_plugin::{Handler, Peer};
    use tokio::sync::{Mutex, mpsc};

    use crate::link::NodeConn;

    async fn app() -> Arc<App> {
        Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        })
    }

    /// A linked node's end, answering `link/pages_serve` and keeping what it was asked.
    #[derive(Default)]
    struct FakeNode {
        served: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl Handler for FakeNode {
        async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
            assert_eq!(method, l::method::PAGES_SERVE);
            self.served.lock().await.push(p);
            Ok(serde_json::json!({}))
        }
    }

    struct Nothing;
    #[async_trait]
    impl Handler for Nothing {
        async fn request(&self, _peer: &Peer, method: &str, _p: Value) -> Result<Value, RpcError> {
            Err(RpcError::method_not_found(method))
        }
    }

    /// Links `node` to the control plane, as `link::run` does.
    async fn link(app: &App, node: &str) -> Arc<FakeNode> {
        let (a_tx, a_rx) = mpsc::channel(16);
        let (b_tx, b_rx) = mpsc::channel(16);
        let fake = Arc::new(FakeNode::default());
        let (control_side, _) = fleetlayer_plugin::connect_frames(a_rx, b_tx, Arc::new(Nothing));
        let (_node_side, _) = fleetlayer_plugin::connect_frames(b_rx, a_tx, fake.clone());
        app.links.nodes.lock().await.insert(("default".into(), node.into()), NodeConn { peer: control_side, conn: 0 });
        fake
    }

    fn push(space: &str, name: &str) -> PagesOpen {
        PagesOpen {
            space: space.into(),
            name: name.into(),
            caller: "qa".into(),
            new: false,
            date: Some("2026-11-27".into()),
            justification: None,
        }
    }

    #[tokio::test]
    async fn a_second_node_claiming_a_linked_nodes_space_is_refused() {
        let app = app().await;
        link(&app, "home").await;
        link(&app, "vps").await;
        assert!(claim(&app, "default", "home", &["personal".into(), "family".into()]).await.is_empty());
        assert_eq!(claim(&app, "default", "vps", &["personal".into(), "work".into()]).await, ["personal"]);
        let map = app.links.pages.lock().await.clone();
        assert_eq!(map[&("default".into(), "personal".into())], "home");
        assert_eq!(map[&("default".into(), "work".into())], "vps");
        drop(map);
        // A node's new list replaces its old one.
        claim(&app, "default", "home", &["personal".into()]).await;
        assert!(!app.links.pages.lock().await.contains_key(&("default".into(), "family".into())));
        // Once home is gone, the space can move.
        app.links.nodes.lock().await.remove(&("default".into(), "home".into()));
        assert!(claim(&app, "default", "vps", &["personal".into(), "work".into()]).await.is_empty());
        assert!(!claim(&app, "default", "vps", &["Bad Name".into()]).await.is_empty());
    }

    #[tokio::test]
    async fn a_push_asks_for_write_on_the_page_then_opens_a_stream_to_the_spaces_node() {
        let app = app().await;
        crate::db::enrol(&app.db, "default", "laptop").await.unwrap();
        crate::db::enrol(&app.db, "default", "home").await.unwrap();
        crate::people::agent_admit(&app.db, "default", "qa@laptop", "admin").await.unwrap();
        let home = link(&app, "home").await;
        claim(&app, "default", "home", &["personal".into()]).await;

        // Fleetlayer's default asks before an agent writes a page.
        let e = open(&app, "default", "laptop", push("personal", "signing-qa")).await.unwrap_err();
        assert!(e.message.to_lowercase().contains("approv"), "{}", e.message);
        assert!(home.served.lock().await.is_empty());

        // A fleet that never asks (`ask: []`) pushes straight away.
        crate::authz::store(&app.db, "default", "version: 2\ndefault: allow\nask: []\n").await.unwrap();
        let opened = open(&app, "default", "laptop", push("personal", "signing-qa")).await.unwrap();
        let served = home.served.lock().await.clone();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0]["streamId"], opened.stream_id);
        assert_eq!(served[0]["caller"], "qa@laptop");
        assert_eq!(served[0]["date"], "2026-11-27");

        // A space nobody hosts, a node that's gone, and names that can't be folders.
        let e = open(&app, "default", "laptop", push("work", "x")).await.unwrap_err();
        assert!(e.message.contains("no pages service has a space called work"), "{}", e.message);
        app.links.nodes.lock().await.remove(&("default".into(), "home".into()));
        let e = open(&app, "default", "laptop", push("personal", "x")).await.unwrap_err();
        assert!(e.message.contains("the pages node for personal (home) is offline"), "{}", e.message);
        assert!(open(&app, "default", "laptop", push("personal", "../etc")).await.is_err());
        assert!(open(&app, "default", "laptop", push("Personal", "x")).await.is_err());
    }

    #[tokio::test]
    async fn a_policy_can_keep_an_agent_out_of_a_space() {
        let app = app().await;
        crate::db::enrol(&app.db, "default", "laptop").await.unwrap();
        crate::people::agent_admit(&app.db, "default", "triage@laptop", "admin").await.unwrap();
        link(&app, "home").await;
        claim(&app, "default", "home", &["personal".into()]).await;
        let policy = "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"triage@*\", can: write, to: \"page:personal/*\" }\n";
        crate::authz::store(&app.db, "default", policy).await.unwrap();
        let mut req = push("personal", "x");
        req.caller = "triage".into();
        let e = open(&app, "default", "laptop", req).await.unwrap_err();
        assert!(e.message.contains("deny[0]"), "{}", e.message);
    }
}
