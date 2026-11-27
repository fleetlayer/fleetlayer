//! Monitoring on the laptop, the job Gatus does on a server: every service's health path, the node's own judgement of
//! its health, and heartbeats pushed the way Gatus's external endpoints take them. Each change (down, recovered) is an
//! alert: a desktop notification and a Fleetlayer message to the chosen session, never only a log.
//!
//! A service that was announced and no longer is (its plugin stopped) counts as down, so killing a plugin shows here
//! even before the node restarts it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::registry::Registry;

/// Failures in a row before a check is down.
const DOWN_AFTER: u32 = 2;
const EVERY: Duration = Duration::from_secs(30);
/// A service missing this long stops being checked.
const FORGET_AFTER_HOURS: i64 = 24;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Node,
    Service,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Check {
    /// None until the first verdict.
    pub up: Option<bool>,
    pub since: Option<Timestamp>,
    pub last_ok: Option<Timestamp>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub fails: u32,
    /// Heartbeats: the last push.
    pub last_push: Option<Timestamp>,
    /// Services: when it was last in the registry.
    pub last_seen: Option<Timestamp>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Keyed by (kind, name).
    pub checks: BTreeMap<String, (Kind, String, Check)>,
    /// Alert messages the node couldn't take yet (it was down).
    #[serde(default)]
    pub queued: Vec<String>,
}

fn key(kind: &Kind, name: &str) -> String {
    format!("{kind:?}:{name}").to_lowercase()
}

pub struct Monitor {
    pub reg: Arc<Registry>,
    pub state: Mutex<State>,
    started: Timestamp,
    http: reqwest::Client,
}

/// What a check's latest result does to it: an alert text when its state changes.
pub fn record(c: &mut Check, label: &str, result: Result<(), String>, now: Timestamp) -> Option<String> {
    match result {
        Ok(()) => {
            c.fails = 0;
            c.last_ok = Some(now);
            c.last_error = None;
            let was = c.up.replace(true);
            if was != Some(true) {
                c.since = Some(now);
            }
            (was == Some(false)).then(|| format!("{label} is back up."))
        }
        Err(e) => {
            c.fails += 1;
            c.last_error = Some(e.clone());
            if c.fails >= DOWN_AFTER && c.up != Some(false) {
                c.up = Some(false);
                c.since = Some(now);
                Some(format!("{label} is down: {e}"))
            } else {
                None
            }
        }
    }
}

impl Monitor {
    pub fn new(reg: Arc<Registry>) -> Arc<Monitor> {
        let path = reg.cfg.state_dir().join("checks.json");
        let state = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client builds");
        Arc::new(Monitor { reg, state: Mutex::new(state), started: Timestamp::now(), http })
    }

    fn save(&self, s: &State) {
        let dir = self.reg.cfg.state_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_vec_pretty(s) {
            let tmp = dir.join("checks.json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, dir.join("checks.json"));
            }
        }
    }

    pub async fn run(self: Arc<Self>) {
        // Let the node announce its services first.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            self.tick().await;
            tokio::time::sleep(EVERY).await;
        }
    }

    /// One round of every check.
    pub async fn tick(&self) {
        let now = Timestamp::now();
        let routes = self.reg.routes().await;
        let mut results: Vec<(Kind, String, Result<(), String>)> = vec![];

        let probes = routes.iter().filter_map(|r| {
            let health = r.health.as_ref()?;
            let url = format!("{}{}", r.url.trim_end_matches('/'), health);
            let http = self.http.clone();
            let name = r.name.clone();
            Some(async move {
                let res = match http.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => Ok(()),
                    Ok(resp) => Err(format!("{url} answered {}", resp.status())),
                    Err(e) => Err(format!("{url}: {}", crate::error_chain(&e))),
                };
                (name, res)
            })
        });
        for (name, res) in futures_join(probes).await {
            results.push((Kind::Service, name, res));
        }
        if self.reg.cfg.node {
            results.push((Kind::Node, "node".into(), crate::node::health(&self.reg.cfg.node_socket()).await));
        }

        let mut alerts = vec![];
        let mut s = self.state.lock().await;
        for (kind, name, res) in results {
            let k = key(&kind, &name);
            let entry = s.checks.entry(k).or_insert_with(|| (kind.clone(), name.clone(), Check::default()));
            if kind == Kind::Service {
                entry.2.last_seen = Some(now);
            }
            alerts.extend(record(&mut entry.2, &label(&kind, &name), res, now));
        }
        // Services checked before and no longer in the registry, and heartbeats that stopped.
        let live: Vec<String> = routes.iter().filter(|r| r.health.is_some()).map(|r| r.name.clone()).collect();
        let mut forget = vec![];
        for (k, (kind, name, c)) in s.checks.iter_mut() {
            match kind {
                Kind::Service if !live.contains(name) => {
                    let seen = c.last_seen.unwrap_or(now);
                    if now.duration_since(seen).as_hours() >= FORGET_AFTER_HOURS
                        || self.reg.cfg.services.get(name).is_some_and(|s| s.hidden)
                    {
                        forget.push(k.clone());
                        continue;
                    }
                    let why = "it isn't in the registry any more (is its plugin running?)".to_string();
                    alerts.extend(record(c, &label(kind, name), Err(why), now));
                }
                _ => {}
            }
        }
        for k in forget {
            s.checks.remove(&k);
        }
        for (name, hb) in &self.reg.cfg.heartbeats {
            let k = key(&Kind::Heartbeat, name);
            let entry = s.checks.entry(k).or_insert_with(|| (Kind::Heartbeat, name.clone(), Check::default()));
            let last = entry.2.last_push.unwrap_or(self.started);
            let late = now.duration_since(last).as_secs() > (hb.every.max(10) * 2) as i64;
            if late {
                let why = format!("no heartbeat since {}", entry.2.last_push.map_or("start".into(), |t| t.to_string()));
                alerts.extend(record(&mut entry.2, &label(&Kind::Heartbeat, name), Err(why), now));
            }
        }
        self.save(&s);
        drop(s);
        self.alert_all(alerts).await;
    }

    /// A heartbeat push. `success: false` counts at once.
    pub async fn push(&self, name: &str, success: bool, error: Option<String>) {
        let now = Timestamp::now();
        let mut s = self.state.lock().await;
        let entry = s
            .checks
            .entry(key(&Kind::Heartbeat, name))
            .or_insert_with(|| (Kind::Heartbeat, name.to_string(), Check::default()));
        entry.2.last_push = Some(now);
        let res =
            if success { Ok(()) } else { Err(error.filter(|e| !e.is_empty()).unwrap_or("reported failure".into())) };
        let mut alert = record(&mut entry.2, &label(&Kind::Heartbeat, name), res.clone(), now);
        // A reported failure is a verdict, not a missed probe: down at once.
        if let Err(e) = res
            && entry.2.up != Some(false)
        {
            entry.2.up = Some(false);
            entry.2.since = Some(now);
            alert = Some(format!("{} is down: {e}", label(&Kind::Heartbeat, name)));
        }
        self.save(&s);
        drop(s);
        self.alert_all(alert.into_iter().collect()).await;
    }

    async fn alert_all(&self, alerts: Vec<String>) {
        let cfg = &self.reg.cfg;
        for a in &alerts {
            tracing::warn!(alert = a.as_str());
            if cfg.alerts.desktop {
                crate::notify::desktop(a);
            }
        }
        let Some(session) = cfg.alerts.session.clone() else { return };
        let mut s = self.state.lock().await;
        let page = cfg.address(crate::registry::STATUS);
        s.queued.extend(alerts.into_iter().map(|a| format!("Fleetlayer gateway: {a} Status: {page}")));
        let mut kept = vec![];
        let queued = std::mem::take(&mut s.queued);
        let mut node_down = false;
        for text in queued {
            if node_down {
                kept.push(text);
                continue;
            }
            if let Err(e) = crate::node::send(&cfg.node_socket(), &session, &text).await {
                tracing::warn!("alert to {session} not sent yet: {e}");
                node_down = true;
                kept.push(text);
            }
        }
        // Keep the newest 50: an alert storm while the node is down shouldn't grow without end.
        let skip = kept.len().saturating_sub(50);
        s.queued = kept.into_iter().skip(skip).collect();
        self.save(&s);
    }
}

pub fn label(kind: &Kind, name: &str) -> String {
    match kind {
        Kind::Node => "The node".into(),
        Kind::Service => format!("Service {name}"),
        Kind::Heartbeat => format!("Heartbeat {name}"),
    }
}

async fn futures_join<F, T>(it: impl Iterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handles: Vec<_> = it.map(tokio::spawn).collect();
    let mut out = vec![];
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_after_two_failures_and_back_up_once() {
        let mut c = Check::default();
        let now = Timestamp::now();
        assert_eq!(record(&mut c, "X", Ok(()), now), None);
        assert_eq!(c.up, Some(true));
        assert_eq!(record(&mut c, "X", Err("a".into()), now), None);
        assert_eq!(record(&mut c, "X", Err("b".into()), now).unwrap(), "X is down: b");
        assert_eq!(record(&mut c, "X", Err("c".into()), now), None);
        assert_eq!(record(&mut c, "X", Ok(()), now).unwrap(), "X is back up.");
        // Down from the start alerts too; first success from unknown doesn't.
        let mut c = Check::default();
        record(&mut c, "Y", Err("a".into()), now);
        assert!(record(&mut c, "Y", Err("a".into()), now).is_some());
    }
}
