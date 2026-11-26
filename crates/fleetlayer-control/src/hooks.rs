//! The webhook receiver, on its own listener so only it needs to face the internet (behind a Cloudflare Tunnel or
//! similar, for a self-hosted control plane). `POST /hooks/<endpoint id>/<secret>` turns the request into an event
//! on the endpoint's channel. It answers 200 whatever happens next, so a sender's own failure alerts don't pile up; a
//! wrong ID or secret is a 404.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use fleetlayer_protocol::ext::Event;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::App;
use crate::db;

const MAX_BODY: usize = 256 * 1024;
/// Events from one endpoint in a minute before the rest are dropped (a sender in a loop).
const PER_MINUTE: usize = 120;

#[derive(Default)]
pub struct Limits {
    recent: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Limits {
    async fn allow(&self, id: &str) -> bool {
        let mut recent = self.recent.lock().await;
        let hits = recent.entry(id.to_string()).or_default();
        let now = Instant::now();
        hits.retain(|t| now.duration_since(*t).as_secs() < 60);
        if hits.len() >= PER_MINUTE {
            return false;
        }
        hits.push(now);
        true
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/hooks/{id}/{secret}", post(receive))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(app)
}

async fn receive(
    State(app): State<Arc<App>>,
    Path((id, secret)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let (tenant, endpoint) = match db::endpoint_for(&app.db, &id, &secret).await {
        Ok(Some(e)) => e,
        Ok(None) => return StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("looking up endpoint {id}: {e}");
            return StatusCode::OK;
        }
    };
    if !app.hook_limits.allow(&id).await {
        tracing::warn!(endpoint = %endpoint.label, "more than {PER_MINUTE} events in a minute; dropping");
        return StatusCode::OK;
    }
    let _ = db::endpoint_hit(&app.db, &id).await;
    let event = to_event(&endpoint.channel, &headers, &query, &body);
    let source = format!("{} webhook", endpoint.label);
    let app2 = app.clone();
    // Deliver in the background: the sender gets its 200 at once.
    tokio::spawn(async move {
        if let Err(e) = crate::route::publish(&app2, &tenant, event, &source).await {
            tracing::warn!("publishing a webhook event: {e}");
        }
    });
    StatusCode::OK
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// Read a webhook as an event: GitHub's by its headers, anything else generically.
pub fn to_event(channel: &str, headers: &HeaderMap, query: &HashMap<String, String>, body: &[u8]) -> Event {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let body_value = json.clone().unwrap_or_else(|| Value::String(String::from_utf8_lossy(body).into_owned()));
    if let (Some(kind), Some(j)) = (header(headers, "x-github-event"), json.as_ref()) {
        let (subject, summary) = github(kind, j);
        return Event {
            channel: channel.into(),
            kind: format!("github.{kind}"),
            subject: query.get("subject").cloned().or(subject),
            at: jiff::Timestamp::now(),
            dedupe_key: header(headers, "x-github-delivery").map(|d| format!("github:{d}")),
            summary,
            body: body_value,
        };
    }
    let field = |k: &str| json.as_ref().and_then(|j| j.get(k)).and_then(Value::as_str).map(str::to_string);
    let summary = field("summary")
        .or_else(|| field("text"))
        .or_else(|| field("message"))
        .or_else(|| field("title"))
        .unwrap_or_else(|| {
            let s = String::from_utf8_lossy(body);
            s.chars().take(200).collect::<String>().trim().to_string()
        });
    Event {
        channel: channel.into(),
        kind: query.get("kind").cloned().or_else(|| field("kind")).unwrap_or_else(|| "webhook".into()),
        subject: query
            .get("subject")
            .cloned()
            .or_else(|| header(headers, "x-fleetlayer-subject").map(str::to_string))
            .or_else(|| field("subject")),
        at: jiff::Timestamp::now(),
        dedupe_key: header(headers, "x-fleetlayer-dedupe").map(str::to_string).or_else(|| field("dedupe_key")),
        summary: if summary.is_empty() { "(empty)".into() } else { summary },
        body: body_value,
    }
}

/// GitHub: the subject is `<owner/repo>@<branch>` where the event has a branch, and a one-line summary.
fn github(kind: &str, j: &Value) -> (Option<String>, String) {
    let s = |p: &str| j.pointer(p).and_then(Value::as_str);
    let repo = s("/repository/full_name");
    let branch = s("/check_suite/head_branch")
        .or_else(|| s("/check_run/check_suite/head_branch"))
        .or_else(|| s("/workflow_run/head_branch"))
        .or_else(|| s("/pull_request/head/ref"))
        .or_else(|| s("/ref").map(|r| r.strip_prefix("refs/heads/").unwrap_or(r)));
    let subject = match (repo, branch) {
        (Some(r), Some(b)) => Some(format!("{r}@{b}")),
        (Some(r), None) => Some(r.to_string()),
        _ => None,
    };
    let action = s("/action");
    let conclusion =
        s("/check_suite/conclusion").or_else(|| s("/check_run/conclusion")).or_else(|| s("/workflow_run/conclusion"));
    let name = s("/workflow_run/name").or_else(|| s("/check_run/name"));
    let mut summary = kind.to_string();
    for part in [name, action, conclusion].into_iter().flatten() {
        summary.push(' ');
        summary.push_str(part);
    }
    if let Some(subj) = &subject {
        summary.push_str(&format!(" on {subj}"));
    }
    (subject, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_workflow_run_names_its_branch() {
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "workflow_run".parse().unwrap());
        h.insert("x-github-delivery", "abc".parse().unwrap());
        let body = serde_json::json!({"action":"completed","repository":{"full_name":"fleetlayer/fleetlayer"},
            "workflow_run":{"name":"CI","head_branch":"fix-login","conclusion":"failure"}});
        let e = to_event("ci", &h, &HashMap::new(), body.to_string().as_bytes());
        assert_eq!(e.kind, "github.workflow_run");
        assert_eq!(e.subject.as_deref(), Some("fleetlayer/fleetlayer@fix-login"));
        assert_eq!(e.summary, "workflow_run CI completed failure on fleetlayer/fleetlayer@fix-login");
        assert_eq!(e.dedupe_key.as_deref(), Some("github:abc"));
    }

    #[test]
    fn a_generic_webhook_uses_its_fields_or_its_text() {
        let q: HashMap<String, String> = [("subject".to_string(), "disk".to_string())].into();
        let e = to_event("alerts", &HeaderMap::new(), &q, br#"{"message":"disk 95% on server","kind":"alert"}"#);
        assert_eq!(
            (e.kind.as_str(), e.summary.as_str(), e.subject.as_deref()),
            ("alert", "disk 95% on server", Some("disk"))
        );
        let e = to_event("alerts", &HeaderMap::new(), &HashMap::new(), b"plain text alert");
        assert_eq!(e.summary, "plain text alert");
        assert_eq!(e.body, Value::String("plain text alert".into()));
    }
}
