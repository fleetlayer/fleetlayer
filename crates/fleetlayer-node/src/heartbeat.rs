//! The node's heartbeat: every `every` seconds it judges its own health and reports it to a URL, the way Gatus's
//! external endpoints take results (`?success=true|false&error=…`, with a bearer token). Healthy means every enabled
//! plugin is running, no sync has been refused for over an hour (`sync.rs`), every remote MCP server it
//! shares answers with its credentials (`gate.rs`) and, when a control plane is configured, the
//! link to it is up. So the check fails when the node is up but has stopped doing its job, not only when it stops; and
//! when the node dies, the heartbeat stops and the monitor's own timeout raises the alert.

use std::sync::Arc;
use std::time::Duration;

use crate::Node;
use crate::config::HeartbeatConfig;
use crate::plugins::State;

/// None when healthy, else why not.
pub async fn health(node: &Node, has_control: bool) -> Option<String> {
    let mut problems = vec![];
    for p in &node.plugins {
        if !p.config.enabled {
            continue;
        }
        let status = p.status().await;
        match status.state {
            State::Running { .. } => {
                if let Some(problem) = status.problem {
                    problems.push(format!("plugin {} is failing ({problem})", p.name()));
                }
            }
            State::Starting => problems.push(format!("plugin {} is starting", p.name())),
            State::Restarting { reason, .. } => problems.push(format!("plugin {} is restarting ({reason})", p.name())),
            State::Broken { reason } => problems.push(format!("plugin {} is broken ({reason})", p.name())),
        }
    }
    // A sync a person must unblock (an approval, a changed key, an access rule) for over an hour.
    problems.extend(node.sync.problems(Duration::from_secs(3600)).await);
    // A delivered secret redacted from what was leaving the node in the last day: rotate it.
    problems.extend(node.secrets.problems());
    // A role window that didn't close cleanly here (a hook failed, a session wouldn't close).
    problems.extend(node.windows.problems().await);
    // A remote MCP server this node shares that fails its check (down, or refusing the node's credentials).
    problems.extend(node.gate.problems().await);
    if has_control && node.link.peer().await.is_none() {
        problems.push("not linked to the control plane".into());
    }
    if let Some(g) = &node.gateway
        && let Err(e) = gateway_ok(g).await
    {
        problems.push(format!("the gateway at {g} isn't answering ({e})"));
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

/// The gateway's own `/healthz`: the check on the gateway that doesn't depend on the gateway.
async fn gateway_ok(base: &str) -> Result<(), String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().map_err(|e| e.to_string())?;
    let url = format!("{}/healthz", base.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("{url} answered {}", r.status())),
        Err(e) => Err(e.to_string()),
    }
}

/// How often the node tells the control plane how it is (`node.status`).
const STATUS_EVERY: Duration = Duration::from_secs(60);

/// The `node.status` reading: the node's own health (the heartbeat's judgement) and each plugin's state, keyed by the
/// node's name, for the dashboard and anything else that reads the fleet. Names, versions and states only: never a
/// plugin's stderr, command line or config, which can carry paths or secrets.
pub async fn status_reading(node: &Node, has_control: bool) -> serde_json::Value {
    let problem = health(node, has_control).await;
    let mut plugins = vec![];
    for p in &node.plugins {
        let s = p.status().await;
        let (state, since, reason) = match &s.state {
            State::Starting => ("starting", None, None),
            State::Running { since } => ("running", Some(since.to_string()), None),
            State::Restarting { at, reason } => ("restarting", Some(at.to_string()), Some(reason.clone())),
            State::Broken { reason } => ("broken", None, Some(reason.clone())),
        };
        plugins.push(serde_json::json!({
            "name": s.name, "enabled": p.config.enabled, "state": state, "since": since, "reason": reason,
            "version": s.version, "roles": s.roles, "restarts": s.restarts, "problem": s.problem,
            "services": s.services.iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
        }));
    }
    serde_json::json!({
        "kind": "node.status",
        "key": node.name,
        "at": jiff::Timestamp::now(),
        "data": {
            "healthy": problem.is_none(),
            "problem": problem,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "plugins": plugins,
        },
    })
}

/// Sends `node.status` every minute while linked (nothing is held while it isn't: the dashboard shows the node as
/// offline then, from the link itself).
pub async fn report_status(node: Arc<Node>) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        if node.link.peer().await.is_some() {
            let r = status_reading(&node, true).await;
            crate::readings::publish(&node, &r).await;
        }
        tokio::time::sleep(STATUS_EVERY).await;
    }
}

pub async fn run(node: Arc<Node>, cfg: HeartbeatConfig, has_control: bool) {
    // `~` means this user's home, as for the control plane's token; a token that can't be read is said once, loudly,
    // rather than heartbeats going out without it (and being refused).
    let token = cfg.token_file.as_ref().and_then(|f| {
        let f = match f.strip_prefix("~") {
            Ok(rest) => crate::config::home().join(rest),
            Err(_) => f.clone(),
        };
        match std::fs::read_to_string(&f) {
            Ok(t) => Some(t.trim().to_string()),
            Err(e) => {
                tracing::error!("heartbeat: reading the token {}: {e}; sending heartbeats without it", f.display());
                None
            }
        }
    });
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(e) => return tracing::error!("heartbeat: {e}"),
    };
    // A minute's grace at start, so plugins and the link come up before the first report.
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let problem = health(&node, has_control).await;
        let mut url = reqwest::Url::parse(&cfg.url).expect("checked when the config was read");
        url.query_pairs_mut()
            .append_pair("success", if problem.is_none() { "true" } else { "false" })
            .append_pair("error", problem.as_deref().unwrap_or(""));
        let mut req = client.post(url);
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!("heartbeat: {} answered {}", cfg.url, r.status()),
            Err(e) => tracing::warn!("heartbeat: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(cfg.every.max(10))).await;
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_status_reading_says_how_the_node_is_and_names_no_paths() {
        let dir = tempfile::tempdir().unwrap();
        let node = crate::Node::for_test(dir.path());
        let r = super::status_reading(&node, true).await;
        assert_eq!((r["kind"].as_str(), r["key"].as_str()), (Some("node.status"), Some("n")));
        // Not linked, with a control plane configured: unhealthy, and says why.
        assert_eq!(r["data"]["healthy"], false);
        assert!(r["data"]["problem"].as_str().unwrap().contains("not linked"), "{r}");
        assert_eq!(r["data"]["plugins"], serde_json::json!([]));
        assert!(!r.to_string().contains(&dir.path().display().to_string()));
    }
}
