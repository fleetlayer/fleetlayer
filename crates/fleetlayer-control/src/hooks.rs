//! The webhook receiver, on its own listener so only it needs to face the internet (behind a Cloudflare Tunnel or
//! similar, for a self-hosted control plane). `POST /hooks/<endpoint id>/<secret>` turns the request into an event
//! on the endpoint's channel. It answers 200 whatever happens next, so a sender's own failure alerts don't pile up; a
//! wrong ID or secret is a 404, and a GitHub signature that doesn't verify (when the endpoint has a GitHub secret) is
//! a 401.
//!
//! GitHub workflow runs also match subscriptions to their commit (`<owner/repo>@<sha>`), besides their branch, so the
//! session that pushed a commit to a shared branch hears its own results. The runs of each commit are tracked for a
//! day: a commit subscription ends once none of its runs is pending, and one made after a run completed is sent that
//! result straight away.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use fleetlayer_protocol::ext::Event;
use fleetlayer_protocol::rpc::RpcError;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

use crate::App;
use crate::db::{self, CommitRun, Subscription};

const MAX_BODY: usize = 256 * 1024;
/// Events from one endpoint in a minute before the rest are dropped (a sender in a loop).
const PER_MINUTE: usize = 120;

#[derive(Default)]
pub struct Limits {
    recent: Mutex<HashMap<String, Vec<Instant>>>,
    /// Held while a workflow run is recorded and delivered, or a commit subscription made: a subscription must not end
    /// between another run's delivery and its record.
    commits: Mutex<()>,
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
    if let Some(secret) = &endpoint.github_secret
        && !signature_ok(secret, header(&headers, "x-hub-signature-256"), &body)
    {
        tracing::warn!(endpoint = %endpoint.label, "a request whose GitHub signature doesn't verify; refused");
        return StatusCode::UNAUTHORIZED;
    }
    if !app.hook_limits.allow(&id).await {
        tracing::warn!(endpoint = %endpoint.label, "more than {PER_MINUTE} events in a minute; dropping");
        return StatusCode::OK;
    }
    let _ = db::endpoint_hit(&app.db, &id).await;
    let mut event = to_event(&endpoint.channel, &headers, &query, &body);
    // A payload can carry the endpoint's own URL (GitHub's ping has it in hook.config.url): its path holds the secret,
    // which must never reach a session.
    redact_event(&mut event, &secret);
    let source = format!("{} webhook", endpoint.label);
    // Deliver in the background: the sender gets its 200 at once.
    tokio::spawn(handle(app, tenant, endpoint, event, source));
    StatusCode::OK
}

/// GitHub's `X-Hub-Signature-256`: `sha256=<hex HMAC-SHA256 of the body>`.
fn signature_ok(secret: &str, given: Option<&str>, body: &[u8]) -> bool {
    let Some(sig) = given.and_then(|g| g.strip_prefix("sha256=")).and_then(|h| hex::decode(h).ok()) else {
        return false;
    };
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    m.update(body);
    m.verify_slice(&sig).is_ok()
}

async fn handle(app: Arc<App>, tenant: String, endpoint: db::Endpoint, event: Event, source: String) {
    let Some(run) = commit_run(&event) else {
        if let Err(e) = crate::route::publish(&app, &tenant, event, &source).await {
            tracing::warn!("publishing a webhook event: {e}");
        }
        return;
    };
    let channel = endpoint.channel.as_str();
    let commit = format!("{}@{}", run.repo, run.sha);
    let _one_at_a_time = app.hook_limits.commits.lock().await;
    if let Err(e) = db::prune_commits(&app.db).await {
        tracing::warn!("forgetting old workflow runs: {e}");
    }
    if run.status != "completed" {
        if let Err(e) = db::record_run(&app.db, &tenant, channel, &run, None).await {
            tracing::warn!("recording a workflow run: {e}");
        }
        if endpoint.all_actions
            && let Err(e) = crate::route::publish_matching(&app, &tenant, event, &source, &[commit]).await
        {
            tracing::warn!("publishing a webhook event: {e}");
        }
        return;
    }
    let id = match crate::route::publish_matching(&app, &tenant, event, &source, std::slice::from_ref(&commit)).await {
        Ok((Some(id), _)) => id,
        Ok((None, _)) => return,
        Err(e) => {
            tracing::warn!("publishing a webhook event: {e}");
            return;
        }
    };
    let ended = async {
        db::record_run(&app.db, &tenant, channel, &run, Some(id)).await?;
        let (pending, _) = db::commit_state(&app.db, &tenant, channel, &run.repo, &run.sha).await?;
        if pending.is_empty() { db::drop_subject(&app.db, &tenant, channel, &commit).await } else { Ok(vec![]) }
    };
    match ended.await {
        Ok(targets) if !targets.is_empty() => {
            tracing::info!(%commit, ?targets, "every run of the commit completed; its subscriptions ended");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("tracking the runs of {commit}: {e}"),
    }
}

/// A workflow run event's commit and run, from the event's body (see `workflow_run_body`).
fn commit_run(e: &Event) -> Option<CommitRun> {
    if e.kind != "github.workflow_run" {
        return None;
    }
    let b = &e.body;
    let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
    Some(CommitRun {
        repo: s("repository")?,
        sha: s("sha")?,
        run_id: b.get("runId")?.as_i64()?,
        workflow: s("workflow").unwrap_or_else(|| "workflow".into()),
        attempt: b.get("attempt").and_then(Value::as_i64).unwrap_or(1),
        status: s("action")?,
    })
}

/// `<owner/repo>@<sha>` when `subject` names a full commit (40 lowercase hex digits).
pub fn commit_subject(subject: &str) -> Option<(&str, &str)> {
    let (repo, sha) = subject.rsplit_once('@')?;
    let hex = sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (!repo.is_empty() && hex).then_some((repo, sha))
}

/// Subscribe. A subscription to a commit is sent the results its runs already have, and ends at once when none is
/// pending; the answer says what it found: `{commit: {subject, pending: [workflow], delivered: [{summary, ok}],
/// ended}}`.
pub async fn subscribe(app: &App, tenant: &str, s: &Subscription) -> Result<Value, RpcError> {
    let Some((repo, sha)) = s.subject.as_deref().and_then(commit_subject) else {
        db::subscribe(&app.db, tenant, s).await.map_err(RpcError::internal)?;
        return Ok(json!({}));
    };
    let _one_at_a_time = app.hook_limits.commits.lock().await;
    db::subscribe(&app.db, tenant, s).await.map_err(RpcError::internal)?;
    let (pending, done) = db::commit_state(&app.db, tenant, &s.channel, repo, sha).await.map_err(RpcError::internal)?;
    let mut delivered = vec![];
    for id in done.iter().copied() {
        if let Some((e, source)) = db::event(&app.db, id).await.map_err(RpcError::internal)? {
            let r = crate::route::deliver_event(app, tenant, id, &e, &source, s.target.clone()).await;
            delivered.push(json!({"summary": e.summary, "ok": r.ok, "error": r.error}));
        }
    }
    let ended = pending.is_empty() && !done.is_empty();
    if ended {
        db::unsubscribe(&app.db, tenant, s).await.map_err(RpcError::internal)?;
    }
    Ok(
        json!({"commit": {"subject": format!("{repo}@{sha}"), "pending": pending, "delivered": delivered, "ended": ended}}),
    )
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
        // Only what a session needs, never GitHub's raw payload: it's long, and full of URLs (a ping's includes the
        // endpoint's own).
        let body_value = match kind {
            "workflow_run" => workflow_run_body(j).unwrap_or_else(|| github_body(kind, j)),
            "ping" => Value::Null,
            _ => github_body(kind, j),
        };
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

/// The fields of any other GitHub event a session can use: the event, action, repository, branch, commit,
/// conclusion and the one page about it.
fn github_body(kind: &str, j: &Value) -> Value {
    let s = |p: &str| j.pointer(p).and_then(Value::as_str);
    let first = |ps: &[&str]| ps.iter().find_map(|p| s(p));
    let mut out = serde_json::Map::new();
    out.insert("event".into(), json!(kind));
    let fields = [
        ("action", first(&["/action"])),
        ("repository", first(&["/repository/full_name"])),
        (
            "branch",
            first(&[
                "/check_suite/head_branch",
                "/check_run/check_suite/head_branch",
                "/pull_request/head/ref",
                "/ref",
            ])
            .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r)),
        ),
        ("sha", first(&["/check_suite/head_sha", "/check_run/head_sha", "/pull_request/head/sha", "/after"])),
        ("conclusion", first(&["/check_suite/conclusion", "/check_run/conclusion"])),
        ("url", first(&["/check_run/html_url", "/pull_request/html_url", "/issue/html_url", "/compare"])),
    ];
    for (k, v) in fields {
        if let Some(v) = v {
            out.insert(k.into(), json!(v));
        }
    }
    Value::Object(out)
}

/// Replace the endpoint's secret wherever it appears in what's delivered: the summary, subject and every string in
/// the body.
fn redact_event(e: &mut Event, secret: &str) {
    const MARK: &str = "[redacted]";
    if secret.is_empty() {
        return;
    }
    fn walk(v: &mut Value, secret: &str) {
        match v {
            Value::String(s) if s.contains(secret) => *s = s.replace(secret, MARK),
            Value::Array(a) => a.iter_mut().for_each(|v| walk(v, secret)),
            Value::Object(o) => o.values_mut().for_each(|v| walk(v, secret)),
            _ => {}
        }
    }
    walk(&mut e.body, secret);
    e.summary = e.summary.replace(secret, MARK);
    if let Some(s) = &mut e.subject {
        *s = s.replace(secret, MARK);
    }
}

/// What a session needs from a workflow run, in place of GitHub's whole payload (tens of kilobytes).
fn workflow_run_body(j: &Value) -> Option<Value> {
    let s = |p: &str| j.pointer(p).and_then(Value::as_str);
    let w = |k: &str| j.pointer(&format!("/workflow_run/{k}")).cloned().unwrap_or(Value::Null);
    Some(json!({
        "repository": s("/repository/full_name")?,
        "branch": w("head_branch"),
        "sha": s("/workflow_run/head_sha")?,
        "commit": s("/workflow_run/head_commit/message").map(first_line),
        "workflow": w("name"),
        "action": s("/action")?,
        "conclusion": w("conclusion"),
        "url": w("html_url"),
        "runId": j.pointer("/workflow_run/id")?.as_i64()?,
        "attempt": w("run_attempt"),
        "event": w("event"),
    }))
}

/// A commit message's title, at most 100 characters.
fn first_line(m: &str) -> String {
    let line = m.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 100 { format!("{}…", line.chars().take(99).collect::<String>()) } else { line.into() }
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
    if kind == "ping" {
        let what = s("/hook/type").unwrap_or("hook");
        let summary = match &subject {
            Some(r) => format!("GitHub's webhook ping: the {what} hook for {r} reaches Fleetlayer"),
            None => format!("GitHub's webhook ping: the {what} hook reaches Fleetlayer"),
        };
        return (subject, summary);
    }
    if kind == "workflow_run"
        && let (Some(subj), Some(sha)) = (&subject, s("/workflow_run/head_sha"))
    {
        // `CI failure on owner/repo@main 1a2b3c4 "Commit title" <run URL>`
        let mut summary = format!(
            "{} {} on {subj} {}",
            s("/workflow_run/name").unwrap_or("workflow"),
            match s("/action") {
                Some("completed") => s("/workflow_run/conclusion").unwrap_or("completed"),
                other => other.unwrap_or("updated"),
            },
            &sha[..sha.len().min(7)],
        );
        if let Some(m) = s("/workflow_run/head_commit/message") {
            summary.push_str(&format!(" \"{}\"", first_line(m)));
        }
        if let Some(url) = s("/workflow_run/html_url") {
            summary.push_str(&format!(" {url}"));
        }
        return (subject, summary);
    }
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

    const SHA: &str = "1a2b3c4d5e6f70811a2b3c4d5e6f70811a2b3c4d";

    fn run(workflow: &str, id: i64, action: &str, conclusion: Option<&str>) -> Value {
        serde_json::json!({"action": action, "repository": {"full_name": "fleetlayer/fleetlayer"},
            "workflow_run": {"id": id, "name": workflow, "head_branch": "main", "head_sha": SHA, "run_attempt": 1,
                "conclusion": conclusion, "event": "push", "html_url": format!("https://github.com/fleetlayer/fleetlayer/actions/runs/{id}"),
                "head_commit": {"message": "Fix the thing\n\nA long body."}}})
    }

    fn github_headers(delivery: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "workflow_run".parse().unwrap());
        h.insert("x-github-delivery", delivery.parse().unwrap());
        h
    }

    #[test]
    fn a_github_workflow_run_names_its_branch_commit_and_run() {
        let body = run("CI", 7, "completed", Some("failure"));
        let e = to_event("ci", &github_headers("abc"), &HashMap::new(), body.to_string().as_bytes());
        assert_eq!(e.kind, "github.workflow_run");
        assert_eq!(e.subject.as_deref(), Some("fleetlayer/fleetlayer@main"));
        assert_eq!(
            e.summary,
            "CI failure on fleetlayer/fleetlayer@main 1a2b3c4 \"Fix the thing\" https://github.com/fleetlayer/fleetlayer/actions/runs/7"
        );
        assert_eq!(e.dedupe_key.as_deref(), Some("github:abc"));
        assert_eq!(e.body["commit"], "Fix the thing");
        assert!(e.body.get("workflow_run").is_none(), "only what a session needs is kept");
        let r = commit_run(&e).unwrap();
        assert_eq!(
            (r.repo.as_str(), r.sha.as_str(), r.run_id, r.status.as_str()),
            ("fleetlayer/fleetlayer", SHA, 7, "completed")
        );

        let e = to_event(
            "ci",
            &github_headers("abd"),
            &HashMap::new(),
            run("CI", 7, "in_progress", None).to_string().as_bytes(),
        );
        assert!(e.summary.starts_with("CI in_progress on fleetlayer/fleetlayer@main 1a2b3c4"), "{}", e.summary);
    }

    #[test]
    fn a_ping_never_passes_on_the_endpoints_url() {
        const SECRET: &str = "5f0c9a1e7b2d4c6f8a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f";
        // The shape of a real ping to a repository hook (identifiers and the secret made up).
        let body = serde_json::json!({
            "zen": "Keep it logically awesome.",
            "hook_id": 717540000,
            "hook": {"type": "Repository", "id": 717540000, "name": "web", "active": true, "events": ["workflow_run"],
                "config": {"content_type": "json", "insecure_ssl": "0", "secret": "********",
                    "url": format!("https://hooks.example.com/hooks/0011223344556677/{SECRET}")},
                "updated_at": "2026-11-27T10:44:52Z", "created_at": "2026-11-27T10:44:52Z",
                "url": "https://api.github.com/repos/fleetlayer/fleetlayer/hooks/717540000",
                "test_url": "https://api.github.com/repos/fleetlayer/fleetlayer/hooks/717540000/test",
                "ping_url": "https://api.github.com/repos/fleetlayer/fleetlayer/hooks/717540000/pings",
                "deliveries_url": "https://api.github.com/repos/fleetlayer/fleetlayer/hooks/717540000/deliveries",
                "last_response": {"code": null, "status": "unused", "message": null}},
            "repository": {"id": 1, "name": "fleetlayer", "full_name": "fleetlayer/fleetlayer",
                "html_url": "https://github.com/fleetlayer/fleetlayer"},
            "sender": {"login": "someone", "id": 2}
        });
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "ping".parse().unwrap());
        h.insert("x-github-delivery", "p1".parse().unwrap());
        let mut e = to_event("ci", &h, &HashMap::new(), body.to_string().as_bytes());
        redact_event(&mut e, SECRET);
        assert_eq!(
            e.summary,
            "GitHub's webhook ping: the Repository hook for fleetlayer/fleetlayer reaches Fleetlayer"
        );
        assert_eq!(e.body, Value::Null, "a ping delivers its summary only");
        let text = crate::route::render_event(&e, "github webhook");
        assert!(!text.contains(SECRET) && !text.contains("/hooks/"), "{text}");
    }

    #[test]
    fn the_endpoint_secret_is_redacted_from_anything_delivered() {
        const SECRET: &str = "abcdef0123456789";
        let body = serde_json::json!({"message": format!("see https://x.example/hooks/id/{SECRET}"),
            "nested": [{"u": format!("{SECRET}{SECRET}")}]});
        let mut e = to_event("alerts", &HeaderMap::new(), &HashMap::new(), body.to_string().as_bytes());
        redact_event(&mut e, SECRET);
        let all = format!("{} {} {:?}", e.summary, e.body, e.subject);
        assert!(!all.contains(SECRET), "{all}");
        assert!(e.summary.ends_with("/hooks/id/[redacted]"), "{}", e.summary);

        // Another GitHub event: a short body, not the raw payload.
        let mut h = HeaderMap::new();
        h.insert("x-github-event", "check_run".parse().unwrap());
        let body = serde_json::json!({"action": "completed", "repository": {"full_name": "o/r"},
            "check_run": {"name": "lint", "conclusion": "failure", "head_sha": "abc", "html_url": "https://github.com/o/r/runs/1",
                "check_suite": {"head_branch": "fix"}, "output": {"text": "a long log"}}});
        let e = to_event("ci", &h, &HashMap::new(), body.to_string().as_bytes());
        assert_eq!(
            e.body,
            serde_json::json!({"event": "check_run", "action": "completed", "repository": "o/r",
            "branch": "fix", "sha": "abc", "conclusion": "failure", "url": "https://github.com/o/r/runs/1"})
        );
    }

    #[test]
    fn github_signatures_and_commit_subjects() {
        // GitHub's documented example: secret "It's a Secret to Everybody", payload "Hello, World!".
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(signature_ok("It's a Secret to Everybody", Some(sig), b"Hello, World!"));
        assert!(!signature_ok("another secret", Some(sig), b"Hello, World!"));
        assert!(!signature_ok("It's a Secret to Everybody", Some(sig), b"Hello, World?"));
        assert!(!signature_ok("It's a Secret to Everybody", None, b"Hello, World!"));
        assert!(!signature_ok("It's a Secret to Everybody", Some("sha1=abc"), b"Hello, World!"));

        assert_eq!(commit_subject(&format!("o/r@{SHA}")), Some(("o/r", SHA)));
        assert_eq!(commit_subject("o/r@main"), None);
        assert_eq!(commit_subject("o/r@1a2b3c4"), None, "a short sha isn't a commit subscription");
        assert_eq!(commit_subject(&format!("o/r@{}", SHA.to_uppercase())), None);
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

    mod commits {
        use std::sync::Arc;

        use async_trait::async_trait;
        use fleetlayer_plugin::{Handler, Peer};
        use serde_json::{Value, json};
        use tokio::sync::{Mutex as AsyncMutex, mpsc};

        use super::*;
        use crate::link::NodeConn;

        /// A linked node that accepts every delivery, keeping (target name, text).
        #[derive(Default)]
        struct Inbox {
            got: AsyncMutex<Vec<(String, String)>>,
        }

        #[async_trait]
        impl Handler for Inbox {
            async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
                assert_eq!(method, fleetlayer_protocol::link::method::DELIVER);
                self.got.lock().await.push((p["name"].as_str().unwrap().into(), p["text"].as_str().unwrap().into()));
                Ok(json!({"via": "harness_channel"}))
            }
        }

        struct Nothing;
        #[async_trait]
        impl Handler for Nothing {
            async fn request(&self, _peer: &Peer, method: &str, _p: Value) -> Result<Value, RpcError> {
                Err(RpcError::method_not_found(method))
            }
        }

        async fn fleet() -> (Arc<App>, Arc<Inbox>, db::Endpoint) {
            let app = Arc::new(App {
                db: db::open_memory().await.unwrap(),
                links: Default::default(),
                hook_limits: Default::default(),
                admin_token_hash: None,
                streams: Default::default(),
                authz: Default::default(),
                audit: crate::audit::Audit::new(None),
            });
            db::enrol(&app.db, "default", "laptop").await.unwrap();
            let (a_tx, a_rx) = mpsc::channel(64);
            let (b_tx, b_rx) = mpsc::channel(64);
            let inbox = Arc::new(Inbox::default());
            let (control_side, _) = fleetlayer_plugin::connect_frames(a_rx, b_tx, Arc::new(Nothing));
            let (node_side, _) = fleetlayer_plugin::connect_frames(b_rx, a_tx, inbox.clone());
            std::mem::forget(node_side);
            app.links
                .nodes
                .lock()
                .await
                .insert(("default".into(), "laptop".into()), NodeConn { peer: control_side, conn: 0 });
            db::set_fallback(&app.db, "default", "ci", Some("fallback@laptop")).await.unwrap();
            let (endpoint, _) = db::add_endpoint(&app.db, "default", "github", "ci", None, false).await.unwrap();
            (app, inbox, endpoint)
        }

        async fn hook(app: &Arc<App>, endpoint: &db::Endpoint, delivery: &str, body: Value) {
            let e = to_event("ci", &github_headers(delivery), &HashMap::new(), body.to_string().as_bytes());
            handle(app.clone(), "default".into(), endpoint.clone(), e, "github webhook".into()).await;
        }

        fn sub(target: &str) -> Subscription {
            Subscription {
                channel: "ci".into(),
                subject: Some(format!("fleetlayer/fleetlayer@{SHA}")),
                target: target.into(),
            }
        }

        async fn taken(inbox: &Inbox) -> Vec<(String, String)> {
            std::mem::take(&mut *inbox.got.lock().await)
        }

        #[tokio::test]
        async fn a_commit_subscription_hears_every_run_then_ends() {
            let (app, inbox, ep) = fleet().await;
            let r = subscribe(&app, "default", &sub("pusher@laptop")).await.unwrap();
            assert_eq!(r["commit"]["pending"], json!([]));
            assert_eq!(r["commit"]["ended"], false, "no runs seen yet: it waits");

            hook(&app, &ep, "d1", run("CI", 1, "requested", None)).await;
            hook(&app, &ep, "d2", run("Site", 2, "requested", None)).await;
            hook(&app, &ep, "d3", run("CI", 1, "in_progress", None)).await;
            assert!(taken(&inbox).await.is_empty(), "only completed runs are delivered");

            hook(&app, &ep, "d4", run("Site", 2, "completed", Some("success"))).await;
            let got = taken(&inbox).await;
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, "pusher", "the commit's subscriber, not the fallback");
            assert!(got[0].1.contains("Site success on fleetlayer/fleetlayer@main 1a2b3c4"), "{}", got[0].1);
            assert_eq!(db::subscriptions(&app.db, "default").await.unwrap().len(), 1, "CI is still pending");

            // A late in_progress for the finished run doesn't make it pending again.
            hook(&app, &ep, "d5", run("Site", 2, "in_progress", None)).await;
            hook(&app, &ep, "d6", run("CI", 1, "completed", Some("failure"))).await;
            let got = taken(&inbox).await;
            assert_eq!(got.len(), 1);
            assert!(got[0].1.contains("CI failure"), "{}", got[0].1);
            assert!(db::subscriptions(&app.db, "default").await.unwrap().is_empty(), "every run completed: it ended");

            // GitHub's retry of the same delivery isn't sent twice.
            hook(&app, &ep, "d6", run("CI", 1, "completed", Some("failure"))).await;
            assert!(taken(&inbox).await.is_empty());
            // Another commit's result, with nobody subscribed, goes to the fallback.
            let mut other = run("CI", 3, "completed", Some("success"));
            other["workflow_run"]["head_sha"] = json!("ffffffffffffffffffffffffffffffffffffffff");
            hook(&app, &ep, "d7", other).await;
            assert_eq!(taken(&inbox).await[0].0, "fallback");
        }

        #[tokio::test]
        async fn a_subscription_after_the_results_gets_them_at_once() {
            let (app, inbox, ep) = fleet().await;
            hook(&app, &ep, "d1", run("CI", 1, "requested", None)).await;
            hook(&app, &ep, "d2", run("Site", 2, "requested", None)).await;
            hook(&app, &ep, "d3", run("Site", 2, "completed", Some("success"))).await;
            assert_eq!(taken(&inbox).await[0].0, "fallback", "nobody had subscribed yet");

            let r = subscribe(&app, "default", &sub("pusher@laptop")).await.unwrap();
            assert_eq!(r["commit"]["pending"], json!(["CI"]));
            assert_eq!(r["commit"]["ended"], false);
            assert_eq!(r["commit"]["delivered"][0]["ok"], true);
            let got = taken(&inbox).await;
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, "pusher");
            assert!(got[0].1.contains("Site success"), "{}", got[0].1);

            hook(&app, &ep, "d4", run("CI", 1, "completed", Some("success"))).await;
            assert_eq!(taken(&inbox).await[0].0, "pusher");

            // Everything finished before this one subscribed: both results now, and it ends at once.
            let r = subscribe(&app, "default", &sub("late@laptop")).await.unwrap();
            assert_eq!(r["commit"]["ended"], true);
            assert_eq!(r["commit"]["delivered"].as_array().unwrap().len(), 2);
            let got = taken(&inbox).await;
            assert_eq!(got.iter().filter(|(t, _)| t == "late").count(), 2);
            assert!(db::subscriptions(&app.db, "default").await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn branch_subscriptions_still_hear_their_branch() {
            let (app, inbox, ep) = fleet().await;
            let branch = Subscription {
                channel: "ci".into(),
                subject: Some("fleetlayer/fleetlayer@main".into()),
                target: "watcher@laptop".into(),
            };
            subscribe(&app, "default", &branch).await.unwrap();
            subscribe(&app, "default", &sub("pusher@laptop")).await.unwrap();
            hook(&app, &ep, "d1", run("CI", 1, "completed", Some("success"))).await;
            let mut names: Vec<String> = taken(&inbox).await.into_iter().map(|(t, _)| t).collect();
            names.sort();
            assert_eq!(names, ["pusher", "watcher"]);
            let left = db::subscriptions(&app.db, "default").await.unwrap();
            assert_eq!(left, vec![branch], "the commit's ended, the branch's stays");
        }
    }
}

fn ci_webhook_failure_test( ) {}
