//! The accounts a Fleetlayer hub manages, with their usage, printed as claude-swap's account list (schema v1), the
//! external account source of menu bar apps such as CodexBar. One binary per provider (`fleetlayer-claude-accounts`),
//! each a [`Provider`]: the reading kinds it reads. The rows, their statuses and the output are the same for all.
//!
//! It asks this machine's node for the newest readings of those kinds (`node/readings`, from every node this one may
//! read) and builds one row per account in the provider's account kind. Nothing is cached and nothing is written: it
//! runs on each refresh, within [`TIMEOUT`].
//!
//! Arguments, as CodexBar passes them (no shell): `--list --json`, `--version`, and `--switch-to <n> --json`, which
//! always fails: the list says `supportsAccountSwitching: false`, because which account a session uses is the
//! rotator's job, per session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Map, Value, json};

/// How long one run may take, well inside CodexBar's 30 seconds.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// A provider: its binary's name and the reading kinds its rows come from. Every kind is keyed by the account's
/// stable ID.
pub struct Provider {
    pub bin: &'static str,
    /// One reading per account, from the hub: `{email, number, signedIn?, signInNeeded?, daysLeft?}`. The rows.
    pub accounts: &'static str,
    /// Usage: `{fiveHour?, sevenDay?, models?, at}`, each window `{pct, resetsAt?}`, `at` in epoch milliseconds.
    pub usage: &'static str,
    /// Whether this machine's copy of the account's token works: `{ok, error?}`.
    pub token_health: Option<&'static str>,
    /// How long the token this machine uses has left: `{daysLeft}`.
    pub token_age: Option<&'static str>,
}

pub const CLAUDE: Provider = Provider {
    bin: "fleetlayer-claude-accounts",
    accounts: "claude.signin_age",
    usage: "claude.usage",
    token_health: Some("claude.token_health"),
    token_age: Some("claude.setup_token_age"),
};

impl Provider {
    fn kinds(&self) -> Vec<&'static str> {
        [Some(self.accounts), Some(self.usage), self.token_health, self.token_age].into_iter().flatten().collect()
    }
}

/// A failure, printed as `{schemaVersion: 1, error: {type, message}}` with a non-zero exit.
#[derive(Debug, PartialEq)]
pub struct Failure {
    pub kind: &'static str,
    pub message: String,
}

fn fail(kind: &'static str, message: impl Into<String>) -> Failure {
    Failure { kind, message: message.into() }
}

impl Failure {
    pub fn json(&self) -> Value {
        json!({"schemaVersion": 1, "error": {"type": self.kind, "message": self.message}})
    }
}

pub fn main(p: &Provider) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (out, code) = match args.as_slice() {
        ["--version"] | ["-V"] => {
            println!("{} {}", p.bin, env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        ["--list", "--json"] | ["--json", "--list"] | ["--list"] => match run(p) {
            Ok(v) => (v, 0),
            Err(f) => (f.json(), 1),
        },
        ["--switch-to", _, "--json"] | ["--switch-to", _] => (
            fail(
                "SwitchingUnsupported",
                "Fleetlayer doesn't switch accounts: each session's account is chosen by the rotator",
            )
            .json(),
            2,
        ),
        _ => (fail("Usage", format!("usage: {} --list --json | --version", p.bin)).json(), 2),
    };
    println!("{out}");
    std::process::exit(code);
}

fn run(p: &Provider) -> Result<Value, Failure> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| fail("Internal", e.to_string()))?;
    rt.block_on(async {
        let socket = default_socket();
        let (here, readings) = tokio::time::timeout(TIMEOUT, fetch(p, &socket))
            .await
            .map_err(|_| fail("Timeout", format!("the node at {} didn't answer in time", socket.display())))??;
        list(p, &readings, &here, jiff::Timestamp::now())
    })
}

/// The node's socket: `$FLEETLAYER_SOCKET`, else the node's default (the same as the `fleetlayer` CLI's).
fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("FLEETLAYER_SOCKET") {
        return PathBuf::from(p);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("fleetlayer/node.sock"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/fleetlayer/node.sock"),
    }
}

struct NoRequests;

#[async_trait]
impl Handler for NoRequests {
    async fn request(&self, _peer: &Peer, method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

/// This node's name, and the newest readings of the provider's kinds.
async fn fetch(p: &Provider, socket: &std::path::Path) -> Result<(String, Vec<Value>), Failure> {
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| {
        fail(
            "NodeUnreachable",
            format!("can't reach the Fleetlayer node at {} ({e}); is fleetlayer-node running?", socket.display()),
        )
    })?;
    let (r, w) = stream.into_split();
    let (peer, _task) = fleetlayer_plugin::connect(r, w, Arc::new(NoRequests));
    let status: Value =
        peer.request("node/status", &json!({})).await.map_err(|e| fail("NodeError", e.message.clone()))?;
    let here = status["node"].as_str().unwrap_or_default().to_string();
    let v: Value = peer.request("node/readings", &json!({"kinds": p.kinds()})).await.map_err(|e| {
        if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND {
            fail("NodeTooOld", "this Fleetlayer node is older than 0.3.4, which lists readings: update it")
        } else {
            fail("NodeError", e.message.clone())
        }
    })?;
    Ok((here, v["readings"].as_array().cloned().unwrap_or_default()))
}

/// When a reading was measured: `data.at` (epoch milliseconds) when it has one, else when it was published.
fn measured(r: &Value) -> Option<jiff::Timestamp> {
    if let Some(ms) = r["data"]["at"].as_f64() {
        return jiff::Timestamp::from_millisecond(ms as i64).ok();
    }
    r["at"].as_str()?.parse().ok()
}

/// The newest reading of `kind` per key, from any node, or only from `node`.
fn newest<'a>(readings: &'a [Value], kind: &str, node: Option<&str>) -> HashMap<&'a str, &'a Value> {
    let mut out: HashMap<&str, &Value> = HashMap::new();
    for r in readings {
        if r["kind"].as_str() != Some(kind) || node.is_some_and(|n| r["node"].as_str() != Some(n)) {
            continue;
        }
        let Some(key) = r["key"].as_str() else { continue };
        if out.get(key).is_none_or(|old| measured(r) > measured(old)) {
            out.insert(key, r);
        }
    }
    out
}

/// ISO 8601 in whole seconds (`2026-11-27T01:05:00Z`), which every ISO 8601 parser reads. None for what isn't a time:
/// CodexBar rejects the whole list over one bad `resetsAt`, so a bad one is left out.
fn iso(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn window(w: &Value) -> Option<Value> {
    let pct = w["pct"].as_f64().filter(|p| p.is_finite())?;
    let mut o = Map::new();
    o.insert("pct".into(), json!(pct));
    if let Some(t) = w["resetsAt"].as_str().and_then(|s| s.parse::<jiff::Timestamp>().ok()) {
        o.insert("resetsAt".into(), json!(iso(t)));
    }
    Some(Value::Object(o))
}

/// claude-swap's `usage` object from a usage reading's data: the windows it has, and each model's own week as
/// `scoped`.
fn usage(d: &Value) -> Value {
    let mut u = Map::new();
    for name in ["fiveHour", "sevenDay"] {
        if let Some(w) = window(&d[name]) {
            u.insert(name.into(), w);
        }
    }
    let scoped: Vec<Value> = d["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, w)| {
            let mut w = window(w)?;
            w["name"] = json!(name);
            Some(w)
        })
        .collect();
    if !scoped.is_empty() {
        u.insert("scoped".into(), Value::Array(scoped));
    }
    Value::Object(u)
}

/// The account list, from readings. `here` is this node's name: whether a token works is this machine's own check.
pub fn list(p: &Provider, readings: &[Value], here: &str, _now: jiff::Timestamp) -> Result<Value, Failure> {
    let accounts = newest(readings, p.accounts, None);
    if accounts.is_empty() {
        return Err(fail(
            "NoAccounts",
            format!("no {} readings: is the hub running, and may this node read it?", p.accounts),
        ));
    }
    let usage_by = newest(readings, p.usage, None);
    let health_by = p.token_health.map(|k| newest(readings, k, Some(here))).unwrap_or_default();
    let age_by = p.token_age.map(|k| newest(readings, k, None)).unwrap_or_default();

    let mut rows: Vec<(u64, Value)> = vec![];
    let mut unnumbered = 0;
    for (key, a) in &accounts {
        let d = &a["data"];
        let Some(number) = d["number"].as_u64().filter(|n| *n > 0) else {
            unnumbered += 1;
            continue;
        };
        if rows.iter().any(|(n, _)| *n == number) {
            continue;
        }
        let used = usage_by.get(key);
        let token_broken = health_by.get(key).is_some_and(|h| h["data"]["ok"] == false)
            || age_by.get(key).is_some_and(|t| t["data"]["daysLeft"].as_i64().is_some_and(|l| l <= 0));
        let sign_in_needed =
            !d["signInNeeded"].is_null() || d["signedIn"] == false || d["daysLeft"].as_i64().is_some_and(|l| l <= 0);
        let status = if token_broken {
            "token_expired"
        } else if sign_in_needed {
            "relogin_required"
        } else if used.is_none() {
            "unavailable"
        } else {
            "ok"
        };
        let mut row = json!({
            "number": number,
            "active": false,
            "usageStatus": status,
            "email": d["email"].as_str().unwrap_or_default(),
            "organizationUuid": "",
            "organizationName": "",
        });
        if let Some(u) = used {
            row["usage"] = usage(&u["data"]);
            if let Some(t) = measured(u) {
                row["usageFetchedAt"] = json!(iso(t));
            }
        }
        rows.push((number, row));
    }
    if rows.is_empty() && unnumbered > 0 {
        return Err(fail("HubTooOld", "the hub's readings carry no account numbers; update the hub's Fleetlayer"));
    }
    rows.sort_by_key(|(n, _)| *n);
    Ok(json!({
        "schemaVersion": 1,
        "activeAccountNumber": null,
        "supportsAccountSwitching": false,
        "accounts": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
    }))
}
