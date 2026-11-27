//! Scrubbing: the node redacts every secret delivered to it from everything it sends to the control plane (messages,
//! events, readings, session titles and summaries, catch-ups), and from what other parts of the node hand it (the MCP
//! gate). The values come from the registry the delivering plugins keep (`fleetlayer_secrets::registry`); a match is
//! replaced by `[REDACTED <reference>]`.
//!
//! A match means the value already reached a session's screen and the model's API, so redacting it isn't the end of
//! it: the node logs it, publishes a `secrets.redacted` reading and an event on the `secrets` channel, and fails its
//! heartbeat for a day with a message that says to rotate the secret.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use fleetlayer_protocol::ext::{self, Event};
use fleetlayer_protocol::link as l;
use fleetlayer_secrets::{Hit, Scrubber, Secret, registry};
use jiff::Timestamp;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Node;

/// How long a redaction keeps the node's health failing.
const LEAK_HOLD: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone)]
struct Leak {
    forms: Vec<&'static str>,
    hits: u64,
    first_at: Timestamp,
    last_at: Timestamp,
    last_context: String,
}

/// A redaction to raise: what was found, and where.
type Alert = (Vec<Hit>, String);

pub struct Secrets {
    home: PathBuf,
    current: RwLock<Arc<Scrubber>>,
    stamp: Mutex<Vec<(String, u64, Option<std::time::SystemTime>)>>,
    /// Secrets removed from this node (`sync::forget_all`), still redacted until the node restarts: its sessions saw
    /// them. Kept in memory only.
    forgotten: Mutex<Vec<Secret>>,
    /// What the registry holds now, by reference, and the files that couldn't be read.
    loaded: Mutex<(Vec<String>, Vec<String>)>,
    leaks: Mutex<BTreeMap<String, Leak>>,
    alerts: mpsc::UnboundedSender<Alert>,
    alerts_rx: Mutex<Option<mpsc::UnboundedReceiver<Alert>>>,
}

impl Secrets {
    pub fn new(home: PathBuf) -> Secrets {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Secrets {
            home,
            current: RwLock::new(Arc::new(Scrubber::default())),
            stamp: Mutex::new(vec![]),
            forgotten: Mutex::new(vec![]),
            loaded: Mutex::new((vec![], vec![])),
            leaks: Mutex::new(BTreeMap::new()),
            alerts: tx,
            alerts_rx: Mutex::new(Some(rx)),
        };
        s.reload();
        s
    }

    /// The scrubber for every secret delivered to this node now (and those forgotten since it started).
    pub fn scrubber(&self) -> Arc<Scrubber> {
        self.current.read().map(|c| c.clone()).unwrap_or_default()
    }

    fn reload(&self) {
        let stamp = registry::stamp(&self.home);
        let (mut secrets, errors) = registry::load(&self.home);
        for e in &errors {
            tracing::warn!("secrets registry: {e}");
        }
        let refs: Vec<String> = secrets.iter().map(|s| s.reference.clone()).collect();
        secrets.extend(self.forgotten.lock().map(|f| f.clone()).unwrap_or_default());
        let scrubber = Arc::new(Scrubber::new(&secrets));
        if !scrubber.too_short().is_empty() {
            tracing::warn!(refs = ?scrubber.too_short(), "secrets too short to redact (under {} characters)",
                fleetlayer_secrets::scrub::MIN_LEN);
        }
        if let Ok(mut c) = self.current.write() {
            *c = scrubber;
        }
        if let Ok(mut s) = self.stamp.lock() {
            *s = stamp;
        }
        if let Ok(mut l) = self.loaded.lock() {
            *l = (refs, errors);
        }
    }

    /// Read the registry again if it changed. Answers whether it did.
    pub fn refresh(&self) -> bool {
        let now = registry::stamp(&self.home);
        if self.stamp.lock().map(|s| *s == now).unwrap_or(false) {
            return false;
        }
        self.reload();
        true
    }

    /// Keep redacting what the registry holds now after it's removed.
    pub fn remember_all(&self) {
        let (secrets, _) = registry::load(&self.home);
        if let Ok(mut f) = self.forgotten.lock() {
            for s in secrets {
                if !f.contains(&s) {
                    f.push(s);
                }
            }
        }
    }

    fn record(&self, hits: &[Hit], context: &str) {
        let now = Timestamp::now();
        if let Ok(mut leaks) = self.leaks.lock() {
            for h in hits {
                let l = leaks.entry(h.reference.clone()).or_insert_with(|| Leak {
                    forms: vec![],
                    hits: 0,
                    first_at: now,
                    last_at: now,
                    last_context: String::new(),
                });
                l.hits += 1;
                l.last_at = now;
                l.last_context = context.to_string();
                if !l.forms.contains(&h.form.label()) {
                    l.forms.push(h.form.label());
                }
            }
        }
        let _ = self.alerts.send((hits.to_vec(), context.to_string()));
    }

    /// Redactions in the last day, as health problems.
    pub fn problems(&self) -> Vec<String> {
        let now = Timestamp::now();
        let leaks = self.leaks.lock().map(|l| l.clone()).unwrap_or_default();
        leaks
            .iter()
            .filter(|(_, l)| now.duration_since(l.last_at).unsigned_abs() < LEAK_HOLD)
            .map(|(r, l)| {
                format!(
                    "the secret {r} was about to leave this node ({}, {} time(s), last at {}); it was redacted, but it \
                     reached a session and the model's API: rotate it",
                    l.last_context,
                    l.hits,
                    l.last_at.strftime("%Y-%m-%d %H:%M UTC")
                )
            })
            .collect()
    }
}

/// `text` with every delivered secret redacted. A hit raises the alert, named by `context` ("a message to ci@laptop",
/// "MCP result from tool:executor@gate to ci@agents"; never a value).
pub fn scrub<'a>(node: &Node, text: &'a str, context: &str) -> Cow<'a, str> {
    scrub_hits(node, text, context).0
}

/// [`scrub`], and what it found.
pub fn scrub_hits<'a>(node: &Node, text: &'a str, context: &str) -> (Cow<'a, str>, Vec<Hit>) {
    scrub_with(&node.secrets, text, context)
}

pub fn scrub_with<'a>(secrets: &Secrets, text: &'a str, context: &str) -> (Cow<'a, str>, Vec<Hit>) {
    let scrubber = secrets.scrubber();
    let (out, hits) = scrubber.scrub(text);
    if !hits.is_empty() {
        let refs: Vec<String> = hits.iter().map(|h| format!("{} ({})", h.reference, h.form.label())).collect();
        tracing::error!(context, secrets = ?refs, "redacted delivered secrets from data leaving this node; rotate them");
        secrets.record(&hits, context);
    }
    (out, hits)
}

/// What a frame to the control plane is, for the alert: its JSON-RPC method.
pub fn frame_context(frame: &str) -> String {
    let method = serde_json::from_str::<Value>(frame)
        .ok()
        .and_then(|v| v["method"].as_str().map(str::to_string))
        .unwrap_or_else(|| "a reply".into());
    format!("{method} to the control plane")
}

/// Notice registry changes every 10 seconds; publish what the node redacts (`secrets.delivered`, references only) when
/// it changes and every half hour; turn each redaction into a reading and an event.
pub async fn run(node: Arc<Node>) {
    let Some(mut rx) = node.secrets.alerts_rx.lock().ok().and_then(|mut r| r.take()) else { return };
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut last_published: Option<(Value, tokio::time::Instant)> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                node.secrets.refresh();
                let data = delivered(&node.secrets);
                let due = last_published.as_ref().is_none_or(|(d, at)| *d != data || at.elapsed() > Duration::from_secs(1800));
                if due {
                    let r = ext::Reading { kind: "secrets.delivered".into(), key: node.name.clone(), at: Timestamp::now(), node: None, data: data.clone() };
                    crate::readings::publish(&node, &json!({"readings": [r]})).await;
                    last_published = Some((data, tokio::time::Instant::now()));
                }
            }
            Some((hits, context)) = rx.recv() => alert(&node, &hits, &context).await,
        }
    }
}

fn delivered(s: &Secrets) -> Value {
    let (refs, errors) = s.loaded.lock().map(|l| l.clone()).unwrap_or_default();
    let scrubber = s.scrubber();
    json!({"references": refs, "redacted": scrubber.len(), "tooShort": scrubber.too_short(), "errors": errors})
}

async fn alert(node: &Node, hits: &[Hit], context: &str) {
    let leaks = node.secrets.leaks.lock().map(|l| l.clone()).unwrap_or_default();
    let now = Timestamp::now();
    let mut readings = vec![];
    for h in hits {
        let Some(l) = leaks.get(&h.reference) else { continue };
        readings.push(ext::Reading {
            kind: "secrets.redacted".into(),
            key: h.reference.clone(),
            at: now,
            node: None,
            data: json!({"reference": h.reference, "forms": l.forms, "hits": l.hits,
                "firstAt": l.first_at.to_string(), "lastAt": l.last_at.to_string(), "lastContext": l.last_context,
                "action": format!("rotate {}: it reached a session and the model's API before it was redacted", h.reference)}),
        });
    }
    crate::readings::publish(node, &json!({"readings": readings})).await;
    let mut refs: Vec<&str> = hits.iter().map(|h| h.reference.as_str()).collect();
    refs.dedup();
    let event = Event {
        channel: "secrets".into(),
        kind: "secrets.redacted".into(),
        subject: Some(format!("-@{}", node.name)),
        at: now,
        dedupe_key: Some(format!("secrets.redacted:{}:{}", node.name, refs.join(","))),
        summary: format!(
            "{} redacted {} from {context}. It reached a session and the model's API first: rotate it.",
            node.name,
            refs.join(", ")
        ),
        body: json!({"node": node.name, "references": refs, "context": context}),
    };
    if let Some(peer) = node.link.peer().await
        && let Err(e) = peer.request_within::<_, Value>(l::method::PUBLISH, &event, Duration::from_secs(15)).await
    {
        tracing::warn!("publishing the secrets.redacted event: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redactions_are_recorded_by_reference_and_fail_health_with_rotate() {
        let d = tempfile::tempdir().unwrap();
        let s = Secret { reference: "op://github/bot/token".into(), value: "ghp_secretsecretsecret".into() };
        registry::register(d.path(), "1password", std::slice::from_ref(&s)).unwrap();
        let secrets = Secrets::new(d.path().to_path_buf());
        let frame = r#"{"jsonrpc":"2.0","method":"link/send","params":{"text":"ghp_secretsecretsecret"}}"#;
        let (out, hits) = scrub_with(&secrets, frame, &frame_context(frame));
        assert!(out.contains("[REDACTED op://github/bot/token]") && !out.contains("ghp_"));
        assert_eq!(hits.len(), 1);
        let p = secrets.problems();
        assert!(p[0].contains("op://github/bot/token") && p[0].contains("link/send to the control plane"), "{p:?}");
        assert!(p[0].contains("rotate it") && !p[0].contains("ghp_"));
    }

    #[test]
    fn registry_changes_are_noticed_and_forgotten_secrets_stay_redacted() {
        let d = tempfile::tempdir().unwrap();
        let secrets = Secrets::new(d.path().to_path_buf());
        assert!(secrets.scrubber().is_empty());
        let s = Secret { reference: "cookie://x/ct0".into(), value: "a-cookie-value-123".into() };
        registry::register(d.path(), "cookies", std::slice::from_ref(&s)).unwrap();
        assert!(secrets.refresh());
        assert!(!secrets.refresh());
        assert!(secrets.scrubber().finds("a-cookie-value-123"));
        secrets.remember_all();
        registry::register(d.path(), "cookies", &[]).unwrap();
        assert!(secrets.refresh());
        assert!(secrets.scrubber().finds("a-cookie-value-123"), "still redacted after it's removed");
    }
}
