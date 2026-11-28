//! fleetlayer-cookies: share a site's cookies from the signed-in browser on one machine (a laptop) with command-line
//! tools on another (a server's agents), such as the `auth_token` and `ct0` cookies an X client needs. The source reads
//! the browser's cookie database for the domains its own config lists, and publishes them as a bundle sealed to each
//! target node's key (`docs/protocol.md` section 8); each target writes them where its tools look. `docs/plugins/cookies.md`
//! has the whole story.
//!
//! ```yaml
//! - name: cookies
//!   command: fleetlayer-cookies
//!   grants: [sync.publish, reading.publish]
//!   config:
//!     share:                         # on the machine with the browser
//!       - { set: x, domains: [x.com], names: [auth_token, ct0], to: [server] }
//!     receive:                       # on each target
//!       - set: x
//!         write:
//!           - { format: env, path: ~/.config/fleetlayer/cookies/x.env, values: { AUTH_TOKEN: auth_token, CT0: ct0 } }
//! ```

mod browser;
mod chrome;
mod disclaim;
mod write;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{
    self, NodeMeta, PluginMeta, Roles, SyncApply, SyncItem, SyncPublish, SyncRole, meta_key,
};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use chrome::{Browser, Cookie, Keys};
use write::Format;

const ITEM: &str = "cookies.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Share {
    set: String,
    /// Host names whose cookies are sent, each with its subdomains (`x.com` covers `api.x.com`).
    domains: Vec<String>,
    /// Only these cookie names (all of the domains' cookies when empty).
    #[serde(default)]
    names: Vec<String>,
    /// Nodes to send to.
    to: Vec<String>,
    /// Where the cookies come from: `browser` (the signed-in Chrome itself, through Playwright MCP's extension, each
    /// read approved by the person; the default on macOS) or `file` (the browser's cookie database; the default
    /// elsewhere).
    #[serde(default)]
    source: Option<Source>,
    /// How to start Playwright MCP for the `browser` source.
    #[serde(default)]
    playwright: browser::Playwright,
    /// The `browser` source reads again after this many days, even when no target asked (each read approved).
    #[serde(default = "fourteen")]
    refresh_days: u64,
    #[serde(default)]
    browser: Browser,
    #[serde(default = "default_profile")]
    profile: String,
    /// The browser's user data folder, when it isn't the usual one.
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    /// The cookie database itself, overriding `browser`'s folder and `profile`.
    #[serde(default)]
    cookies_file: Option<PathBuf>,
    /// Cookies that must be there and unexpired, else nothing is sent and the plugin reports a problem (default:
    /// `names`).
    #[serde(default)]
    require: Option<Vec<String>>,
    #[serde(default = "five")]
    every_minutes: u64,
    /// Send again after this long even when nothing changed, so a target can tell the source is alive.
    #[serde(default = "twelve")]
    resend_hours: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    format: Format,
    path: PathBuf,
    /// For `env`: variable name → cookie name.
    #[serde(default)]
    values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receive {
    set: String,
    /// The source's node, the only one whose bundles for this set are applied. Default: the first node that sent one
    /// (remembered), since any node can run a plugin called cookies and publish a bundle of the same name.
    #[serde(default)]
    from: Option<String>,
    write: Vec<Output>,
    /// Cookies that must be there and unexpired (the ones `env` outputs name are always required).
    #[serde(default)]
    require: Vec<String>,
    /// Report a problem when nothing has arrived for this long.
    #[serde(default = "forty_eight")]
    stale_hours: u64,
}

impl Receive {
    fn required(&self) -> BTreeSet<String> {
        let mut r: BTreeSet<String> = self.require.iter().cloned().collect();
        for o in &self.write {
            r.extend(o.values.values().cloned());
        }
        r
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Source {
    File,
    Browser,
}

impl Share {
    fn source(&self) -> Source {
        self.source.unwrap_or(if cfg!(target_os = "macos") { Source::Browser } else { Source::File })
    }
}

fn fourteen() -> u64 {
    14
}

fn default_profile() -> String {
    "Default".into()
}
fn five() -> u64 {
    5
}
fn twelve() -> u64 {
    12
}
fn forty_eight() -> u64 {
    48
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    share: Vec<Share>,
    #[serde(default)]
    receive: Vec<Receive>,
}

fn expand(home: &Path, p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn set_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A plain host name below a public suffix: never a wildcard, a bare top-level domain or a suffix anyone can register
/// under (`co.uk`, `github.io`), which would share every site below it.
fn domain_ok(d: &str) -> bool {
    let d = d.trim_start_matches('.').to_ascii_lowercase();
    let plain = d.contains('.')
        && d.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    plain && psl::domain_str(&d).is_some()
}

fn env_var_ok(v: &str) -> bool {
    let mut cs = v.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The config with `~` expanded, or why it's wrong.
fn load(home: &Path, raw: &Value) -> Result<Config, String> {
    let mut c: Config = serde_json::from_value(raw.clone()).map_err(|e| format!("fleetlayer-cookies config: {e}"))?;
    let mut sets = BTreeSet::new();
    for s in &mut c.share {
        if !set_name_ok(&s.set) || !sets.insert(s.set.clone()) {
            return Err(format!("share: {:?} isn't a unique set name (a-z, 0-9, - and _)", s.set));
        }
        if s.domains.is_empty() || !s.domains.iter().all(|d| domain_ok(d)) {
            return Err(format!(
                "share {}: domains must be plain host names such as x.com, never a public suffix such as co.uk or github.io",
                s.set
            ));
        }
        if s.to.is_empty() {
            return Err(format!("share {}: no nodes to send to", s.set));
        }
        if s.source() == Source::Browser {
            if s.browser != Browser::Chrome {
                return Err(format!("share {}: the browser source reads Chrome only (or use source: file)", s.set));
            }
            if s.names.is_empty() {
                return Err(format!(
                    "share {}: the browser source sends only the cookies you name: add names (such as [auth_token, ct0])",
                    s.set
                ));
            }
            s.playwright = s.playwright.resolved(home);
        }
        s.user_data_dir = s.user_data_dir.as_deref().map(|p| expand(home, p));
        s.cookies_file = s.cookies_file.as_deref().map(|p| expand(home, p));
    }
    let mut sets = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for r in &mut c.receive {
        if !set_name_ok(&r.set) || !sets.insert(r.set.clone()) {
            return Err(format!("receive: {:?} isn't a unique set name (a-z, 0-9, - and _)", r.set));
        }
        if r.write.is_empty() {
            return Err(format!("receive {}: nothing to write", r.set));
        }
        for o in &mut r.write {
            o.path = expand(home, &o.path);
            if !o.path.is_absolute() || !paths.insert(o.path.clone()) {
                return Err(format!("receive {}: {} must be absolute and written once", r.set, o.path.display()));
            }
            match o.format {
                Format::Env if o.values.is_empty() || !o.values.keys().all(|k| env_var_ok(k)) => {
                    return Err(format!(
                        "receive {}: an env file needs values, variable names to cookie names ({{AUTH_TOKEN: auth_token}})",
                        r.set
                    ));
                }
                Format::Netscape | Format::Json if !o.values.is_empty() => {
                    return Err(format!("receive {}: values are only for the env format", r.set));
                }
                _ => {}
            }
        }
    }
    Ok(c)
}

/// What a bundle carries: one JSON item.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    set: String,
    domains: Vec<String>,
    read_at: Timestamp,
    cookies: Vec<Cookie>,
    /// From the `browser` source: how many days it waits before reading again unasked, so a target doesn't call the
    /// set stale before then. None from the `file` source, which sends at least every `resend_hours`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_days: Option<u64>,
}

/// A cookie without its value, as the state file and readings name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    domain: String,
    name: String,
    expires: Option<i64>,
}

/// What a target keeps about a set it received (`~/.local/state/fleetlayer/cookies/<set>.json`): never values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Received {
    version: u64,
    applied_at: Option<Timestamp>,
    read_at: Option<Timestamp>,
    cookies: Vec<Seen>,
    /// Files this plugin wrote, so it never overwrites one a person made.
    written: Vec<PathBuf>,
    error: Option<String>,
    /// The source's refresh period, when it reads a browser (see `Body::refresh_days`).
    #[serde(default)]
    refresh_days: Option<u64>,
}

/// The earliest expiry among `names` (Unix seconds), what's missing, and what's expired at `now`.
fn check(cookies: &[Seen], names: &BTreeSet<String>, now: i64) -> (Option<i64>, Vec<String>, Vec<(String, i64)>) {
    let mut earliest: Option<i64> = None;
    let (mut missing, mut expired) = (vec![], vec![]);
    for n in names {
        let best = cookies.iter().filter(|c| &c.name == n).max_by_key(|c| c.expires.unwrap_or(i64::MAX));
        match best {
            None => missing.push(n.clone()),
            Some(c) => {
                if let Some(e) = c.expires {
                    earliest = Some(earliest.map_or(e, |x| x.min(e)));
                    if e <= now {
                        expired.push((n.clone(), e));
                    }
                }
            }
        }
    }
    (earliest, missing, expired)
}

fn when(unix: i64) -> String {
    Timestamp::from_second(unix).map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

/// A target's problem with a set, for a person, or None.
fn receive_problem(r: &Receive, st: &Received, now: Timestamp, started: Timestamp) -> Option<String> {
    if let Some(e) = &st.error {
        return Some(format!("cookies {}: {e}", r.set));
    }
    // A browser source reads only every `refresh_days` unasked: stale two days after that.
    let hours = st.refresh_days.map_or(r.stale_hours, |d| r.stale_hours.max(d * 24 + 48));
    let stale = Duration::from_secs(hours * 3600);
    let Some(applied) = st.applied_at else {
        let waited = now.duration_since(started).unsigned_abs();
        return (waited > stale).then(|| {
            format!(
                "cookies {}: nothing has arrived since this plugin started at {}; is the source machine awake, and \
                 its sync to this node approved?",
                r.set,
                when(started.as_second())
            )
        });
    };
    let (_, missing, expired) = check(&st.cookies, &r.required(), now.as_second());
    if !missing.is_empty() {
        return Some(format!("cookies {}: what was sent has no {} cookie", r.set, missing.join(" or ")));
    }
    if let Some((name, at)) = expired.first() {
        return Some(format!(
            "cookies {}: the {name} cookie expired at {}; sign in again in the browser on the source machine",
            r.set,
            when(*at)
        ));
    }
    let since = now.duration_since(applied).unsigned_abs();
    (since > stale).then(|| {
        format!(
            "cookies {}: nothing has arrived since {} (over {} hours); is the source machine awake?",
            r.set,
            when(applied.as_second()),
            r.stale_hours
        )
    })
}

/// Make `set`'s files from `body`. A file that exists and wasn't written by this plugin is left alone and is an
/// error. Answers what was written and the errors.
fn apply(r: &Receive, body: &Body, written_before: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let mut written = vec![];
    let mut errors = vec![];
    for o in &r.write {
        let mine = written_before.contains(&o.path);
        if !mine && std::fs::symlink_metadata(&o.path).is_ok() {
            errors.push(format!(
                "{} is there already and wasn't written by fleetlayer-cookies; move it away to have it written",
                o.path.display()
            ));
            continue;
        }
        let content = match o.format {
            Format::Netscape => Ok(write::netscape(&body.cookies)),
            Format::Json => Ok(write::json(&body.cookies)),
            Format::Env => write::env(&body.cookies, &o.values),
        };
        match content
            .map_err(|e| format!("{}: {e}", o.path.display()))
            .and_then(|c| write::atomic(&o.path, &c).map_err(|e| format!("writing {}: {e}", o.path.display())))
        {
            Ok(()) => written.push(o.path.clone()),
            Err(e) => {
                if mine {
                    written.push(o.path.clone());
                }
                errors.push(e);
            }
        }
    }
    (written, errors)
}

fn digest(cookies: &[Cookie]) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(cookies).unwrap_or_default());
    hex::encode(h.finalize())
}

struct Plugin {
    home: PathBuf,
    /// This node's name.
    node: String,
    /// The newest request each target made for a set, from its `cookies.receive` reading: (target, set) → (when,
    /// why).
    wanted: Mutex<HashMap<(String, String), (Timestamp, String)>>,
    receive: Vec<Receive>,
    started: Timestamp,
    received: Mutex<HashMap<String, Received>>,
    /// Each source set's problem, None when it sends fine.
    share_problems: Mutex<BTreeMap<String, Option<String>>>,
    /// Each set's last bundle refused for coming from the wrong node, and when.
    refused: Mutex<HashMap<String, (String, Timestamp)>>,
    /// Keys read once per process: a Keychain refusal isn't asked again until the plugin restarts.
    keys: Mutex<HashMap<String, Result<Keys, String>>>,
}

impl Plugin {
    fn state_file(&self, set: &str) -> PathBuf {
        self.home.join(".local/state/fleetlayer/cookies").join(format!("{set}.json"))
    }

    fn save(&self, set: &str, st: &Received) {
        let path = self.state_file(set);
        let text = serde_json::to_string_pretty(st).unwrap_or_default();
        if let Err(e) = write::atomic(&path, &text) {
            tracing::warn!(set, "saving {}: {e}", path.display());
        }
    }

    fn load_state(&self, set: &str) -> Received {
        std::fs::read(self.state_file(set)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
}

struct Shared(Mutex<Option<Arc<Plugin>>>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-cookies runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config = load(&home, &node.config).map_err(RpcError::refused)?;
                let roles = Roles {
                    sync_target: (!config.receive.is_empty()).then(|| SyncRole {
                        bundles: config.receive.iter().map(|r| format!("cookies/{}", r.set)).collect(),
                    }),
                    sync_source: (!config.share.is_empty()).then(|| SyncRole {
                        bundles: config.share.iter().map(|s| format!("cookies/{}", s.set)).collect(),
                    }),
                    ..Default::default()
                };
                let plugin = Arc::new(Plugin {
                    home: home.clone(),
                    node: node.node.clone(),
                    wanted: Mutex::new(HashMap::new()),
                    receive: config.receive.clone(),
                    started: Timestamp::now(),
                    received: Mutex::new(HashMap::new()),
                    share_problems: Mutex::new(BTreeMap::new()),
                    keys: Mutex::new(HashMap::new()),
                    refused: Mutex::new(HashMap::new()),
                });
                {
                    let mut rec = plugin.received.lock().await;
                    for r in &config.receive {
                        rec.insert(r.set.clone(), plugin.load_state(&r.set));
                    }
                }
                let first = self.0.lock().await.replace(plugin.clone()).is_none();
                if first {
                    if config.share.iter().any(|s| s.source() == Source::Browser) {
                        tokio::spawn(subscribe_requests(peer.clone()));
                    }
                    for s in config.share {
                        match s.source() {
                            Source::File => tokio::spawn(share_loop(plugin.clone(), s, home.clone(), peer.clone())),
                            Source::Browser => tokio::spawn(browser_loop(plugin.clone(), s, peer.clone())),
                        };
                    }
                    if roles.sync_target.is_some() || roles.sync_source.is_some() {
                        tokio::spawn(status_loop(plugin.clone(), peer.clone()));
                    }
                }
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(
                            meta_key::PLUGIN,
                            &PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles },
                        )),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-cookies".into(),
                        title: Some("Cookies".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SYNC_APPLY => {
                let a: SyncApply = params(p)?;
                let plugin = self.0.lock().await.clone().ok_or_else(|| RpcError::refused("not initialised"))?;
                let set = a.bundle.strip_prefix("cookies/").unwrap_or_default().to_string();
                let r = plugin
                    .receive
                    .iter()
                    .find(|r| r.set == set)
                    .cloned()
                    .ok_or_else(|| RpcError::not_here(format!("no receive entry for {}", a.bundle)))?;
                let pins = plugin.home.join(".local/state/fleetlayer/cookies/sources.json");
                if let Err(e) = signer_of(&a).and_then(|signer| {
                    fleetlayer_secrets::pin::check(&pins, &a.bundle, a.from.as_deref(), r.from.as_deref(), signer.as_deref())
                }) {
                    tracing::error!("{e}");
                    plugin.refused.lock().await.insert(set.clone(), (e.clone(), Timestamp::now()));
                    return Ok(json!({"applied": false, "error": e}));
                }
                let body: Body = match a.items.iter().find(|i| i.name == ITEM).map(|i| serde_json::from_str(&i.content))
                {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => return Ok(json!({"applied": false, "error": format!("{ITEM}: {e}")})),
                    None => return Ok(json!({"applied": false, "error": format!("the bundle has no {ITEM}")})),
                };
                let mut rec = plugin.received.lock().await;
                let st = rec.entry(set.clone()).or_default();
                let (written, errors) = apply(&r, &body, &st.written);
                st.written = written;
                st.version = a.version;
                st.read_at = Some(body.read_at);
                st.refresh_days = body.refresh_days;
                st.cookies = body
                    .cookies
                    .iter()
                    .map(|c| Seen { domain: c.domain.clone(), name: c.name.clone(), expires: c.expires })
                    .collect();
                if errors.is_empty() {
                    st.applied_at = Some(Timestamp::now());
                    st.error = None;
                    let _ = std::fs::remove_file(request_file(&plugin.home, &set));
                } else {
                    st.error = Some(errors.join("; "));
                }
                let snapshot = st.clone();
                drop(rec);
                plugin.save(&set, &snapshot);
                // The node redacts these values from what it sends, and `fleetlayer secrets forget` knows them.
                let secrets: Vec<fleetlayer_secrets::Secret> = body
                    .cookies
                    .iter()
                    .map(|c| fleetlayer_secrets::Secret {
                        reference: format!("cookie://{set}/{}", c.name),
                        value: c.value.clone(),
                    })
                    .collect();
                if let Err(e) =
                    fleetlayer_secrets::registry::register(&plugin.home, &format!("cookies-{set}"), &secrets)
                {
                    tracing::warn!(set, "listing the cookies in the secrets registry: {e}");
                }
                tracing::info!(set, version = a.version, cookies = body.cookies.len(), domains = ?body.domains,
                    files = snapshot.written.len(), errors = errors.len(), "received");
                if errors.is_empty() {
                    Ok(json!({"applied": true, "written": snapshot.written.len()}))
                } else {
                    Ok(json!({"applied": false, "error": errors.join("; ")}))
                }
            }
            ext::method::SYNC_FORGET => {
                // A role's window ended: every file written and every registry list goes.
                let plugin = self.0.lock().await.clone().ok_or_else(|| RpcError::refused("not initialised"))?;
                let mut errors = vec![];
                let mut rec = plugin.received.lock().await;
                for r in &plugin.receive {
                    let st = rec.entry(r.set.clone()).or_default();
                    for (p, e) in fleetlayer_secrets::write::remove_stale(&st.written, &[]) {
                        errors.push(format!("removing {}: {e}", p.display()));
                    }
                    if let Err(e) =
                        fleetlayer_secrets::registry::register(&plugin.home, &format!("cookies-{}", r.set), &[])
                    {
                        errors.push(format!("the secrets registry: {e}"));
                    }
                    *st = Received::default();
                    plugin.save(&r.set, st);
                }
                drop(rec);
                tracing::warn!(errors = errors.len(), "forgot every cookie delivered to this node");
                if errors.is_empty() {
                    Ok(json!({"forgotten": true}))
                } else {
                    Ok(json!({"forgotten": false, "error": errors.join("; ")}))
                }
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method != ext::method::READING_UPDATE {
            return;
        }
        let Some(plugin) = self.0.lock().await.clone() else { return };
        let Ok(r) = serde_json::from_value::<ext::Readings>(p) else { return };
        for reading in r.readings.into_iter().filter(|x| x.kind == "cookies.receive") {
            let (Some(node), Some(at)) =
                (reading.node.clone(), reading.data["requestedAt"].as_str().and_then(|t| t.parse::<Timestamp>().ok()))
            else {
                continue;
            };
            let why = reading.data["requestWhy"].as_str().unwrap_or_default().to_string();
            let mut wanted = plugin.wanted.lock().await;
            let key = (node, reading.key.clone());
            if wanted.get(&key).is_none_or(|(old, _)| *old < at) {
                wanted.insert(key, (at, why));
            }
        }
    }
}

/// The cookie database a share reads.
fn db_path(s: &Share, home: &Path) -> PathBuf {
    match &s.cookies_file {
        Some(f) => f.clone(),
        None => {
            let base = s.user_data_dir.clone().unwrap_or_else(|| s.browser.user_data_dir(home));
            chrome::cookies_file(&base.join(&s.profile))
        }
    }
}

/// Read a share's cookies, or say why not, for a person.
async fn read_share(plugin: &Plugin, s: &Share, home: &Path) -> Result<Vec<Cookie>, String> {
    let label = format!("{:?}", s.browser);
    let keys = {
        let mut cache = plugin.keys.lock().await;
        if !cache.contains_key(&label) {
            let browser = s.browser;
            let got = tokio::task::spawn_blocking(move || chrome::keys(browser))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            cache.insert(label.clone(), got);
        }
        cache[&label].clone()?
    };
    let db = db_path(s, home);
    let domains = s.domains.clone();
    let mut cookies = tokio::task::spawn_blocking(move || chrome::read(&db, &keys, &domains))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("reading {}'s cookies: {e}", s.browser.label()))?;
    if !s.names.is_empty() {
        cookies.retain(|c| s.names.contains(&c.name));
    }
    Ok(cookies)
}

/// Read the set every `every_minutes`; publish when it changed or `resend_hours` passed. Each round is a
/// `cookies.share` reading and sets the set's problem.
async fn share_loop(plugin: Arc<Plugin>, s: Share, home: PathBuf, peer: Peer) {
    let required: BTreeSet<String> = s.require.clone().unwrap_or_else(|| s.names.clone()).into_iter().collect();
    let mut last = String::new();
    let mut published: Option<(u64, Timestamp)> = None;
    tokio::time::sleep(Duration::from_secs(3)).await;
    while !peer.is_closed() {
        let now = Timestamp::now();
        let mut problem = None;
        let mut data = json!({"set": s.set, "domains": s.domains, "to": s.to, "browser": s.browser.label()});
        match read_share(&plugin, &s, &home).await {
            Err(e) => problem = Some(format!("cookies {}: {e}", s.set)),
            Ok(cookies) => {
                let seen: Vec<Seen> = cookies
                    .iter()
                    .map(|c| Seen { domain: c.domain.clone(), name: c.name.clone(), expires: c.expires })
                    .collect();
                let (earliest, missing, expired) = check(&seen, &required, now.as_second());
                data["cookies"] = json!(cookies.len());
                data["earliestExpiry"] = json!(earliest.map(when));
                if cookies.is_empty() {
                    problem = Some(format!(
                        "cookies {}: {} has no cookies for {}; sign in there in {}",
                        s.set,
                        s.browser.label(),
                        s.domains.join(", "),
                        s.browser.label()
                    ));
                } else if !missing.is_empty() {
                    problem = Some(format!(
                        "cookies {}: {} has no {} cookie for {}; sign in there in {}",
                        s.set,
                        s.browser.label(),
                        missing.join(" or "),
                        s.domains.join(", "),
                        s.browser.label()
                    ));
                } else if let Some((name, at)) = expired.first() {
                    problem = Some(format!(
                        "cookies {}: the {name} cookie expired at {}; sign in to {} again in {}",
                        s.set,
                        when(*at),
                        s.domains.join(", "),
                        s.browser.label()
                    ));
                } else {
                    let d = digest(&cookies);
                    let due = published.is_none_or(|(_, at)| {
                        now.duration_since(at).unsigned_abs() >= Duration::from_secs(s.resend_hours.max(1) * 3600)
                    });
                    if d != last || due {
                        let version = now.as_millisecond() as u64;
                        let body = Body {
                            set: s.set.clone(),
                            domains: s.domains.clone(),
                            read_at: now,
                            cookies,
                            refresh_days: None,
                        };
                        let publish = SyncPublish {
                            bundle: format!("cookies/{}", s.set),
                            version,
                            secret: true,
                            targets: s.to.iter().map(|n| ext::SyncTarget { node: n.clone() }).collect(),
                            items: vec![SyncItem {
                                name: ITEM.into(),
                                content: serde_json::to_string(&body).unwrap_or_default(),
                                mode: Some(0o600),
                                link: None,
                            }],
                        };
                        match peer
                            .request_within::<_, Value>(ext::method::SYNC_PUBLISH, &publish, Duration::from_secs(90))
                            .await
                        {
                            Ok(r) => {
                                for f in r["results"].as_array().into_iter().flatten().filter(|x| x["ok"] != true) {
                                    tracing::warn!(set = %s.set, version, target = %f["target"], error = %f["error"],
                                        "a target didn't get this version; the node tries it again");
                                }
                                tracing::info!(set = %s.set, version, domains = ?s.domains, to = ?s.to,
                                    cookies = body.cookies.len(), "sent");
                                last = d;
                                published = Some((version, now));
                            }
                            Err(e) => problem = Some(format!("cookies {}: handing the bundle to the node: {e}", s.set)),
                        }
                    }
                }
            }
        }
        if let Some(p) = &problem {
            tracing::warn!("{p}");
        }
        data["version"] = json!(published.map(|p| p.0));
        data["publishedAt"] = json!(published.map(|p| p.1.to_string()));
        data["readAt"] = json!(now.to_string());
        data["problem"] = json!(problem);
        plugin.share_problems.lock().await.insert(s.set.clone(), problem);
        let reading = ext::Reading { kind: "cookies.share".into(), key: s.set.clone(), at: now, node: None, data };
        peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: vec![reading] });
        tokio::time::sleep(Duration::from_secs(s.every_minutes.max(1) * 60)).await;
    }
}

/// A target's request for a fresh read, written by `fleetlayer-cookies request <set>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    at: Timestamp,
    why: String,
}

fn request_file(home: &Path, set: &str) -> PathBuf {
    home.join(".local/state/fleetlayer/cookies").join(format!("{set}.request"))
}

fn read_request(home: &Path, set: &str) -> Option<Request> {
    std::fs::read(request_file(home, set)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// A reason as one plain line (it reaches the person in an approval request): printable characters, at most 300.
fn plain_line(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
}

/// Requests from targets arrive in their `cookies.receive` readings, from any node.
async fn subscribe_requests(peer: Peer) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    let kinds = json!({"kinds": ["cookies.receive"]});
    if let Err(e) = peer.request::<_, Value>(ext::method::READING_SUBSCRIBE, &kinds).await {
        tracing::warn!("subscribing to cookies.receive readings (grant reading.subscribe): {e}");
    }
}

/// What a `browser` share remembers per target (`~/.local/state/fleetlayer/cookies/share-<set>.json`): never values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivered {
    delivered_at: Option<Timestamp>,
    /// The newest request it has answered.
    answered: Option<Timestamp>,
}

/// Why a `browser` share should read for `target` now, if it should.
fn due(d: &Delivered, wanted: Option<&(Timestamp, String)>, refresh_days: u64, now: Timestamp) -> Option<String> {
    if let Some((at, why)) = wanted
        && d.answered.is_none_or(|a| a < *at)
    {
        return Some(if why.is_empty() { "it asked".into() } else { format!("it asked: {why}") });
    }
    match d.delivered_at {
        None => Some("its first delivery".into()),
        Some(t) if now.duration_since(t).unsigned_abs() >= Duration::from_secs(refresh_days.max(1) * 86_400) => {
            Some(format!("the {refresh_days}-day refresh"))
        }
        Some(_) => None,
    }
}

/// The approval to read the browser for `target` (`sync` on `sync:cookies-from-browser/<set>@<target>`, asked each
/// time by default): Ok(true) to go ahead, Ok(false) while it waits for the person, Err when refused.
async fn approved(peer: &Peer, node: &str, s: &Share, target: &str, why: &str) -> Result<bool, String> {
    let ask = |peek: bool| fleetlayer_protocol::link::AccessCheck {
        tenant: String::new(),
        who: format!("-@{node}"),
        can: "sync".into(),
        to: format!("sync:cookies-from-browser/{}@{target}", s.set),
        why: Some(why.to_string()),
        peek,
    };
    let call = |peek: bool| async move {
        peer.request_within::<_, fleetlayer_protocol::link::AccessDecision>(
            ext::method::ACCESS_CHECK,
            &ask(peek),
            Duration::from_secs(20),
        )
        .await
        .map_err(|e| format!("asking the control plane (grant access.check): {}", e.message))
    };
    // Look first, so an approval for one read is spent only by the read itself.
    let looked = call(true).await?;
    if !looked.allowed {
        if looked.reason.contains("waiting for the person") {
            return Ok(false);
        }
        if !looked.reason.contains("not asked yet") {
            return Err(looked.reason);
        }
    }
    let d = call(false).await?;
    if d.allowed {
        return Ok(true);
    }
    if d.reason.contains("approv") && !d.reason.contains("refused") { Ok(false) } else { Err(d.reason) }
}

/// The `browser` source: reads the signed-in Chrome only when a target is due (it asked, its first delivery, or the
/// refresh), and only once the person approves that read. Each round is a `cookies.share` reading.
async fn browser_loop(plugin: Arc<Plugin>, s: Share, peer: Peer) {
    let state_path = plugin.home.join(".local/state/fleetlayer/cookies").join(format!("share-{}.json", s.set));
    let mut state: BTreeMap<String, Delivered> =
        std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let required: BTreeSet<String> = s.require.clone().unwrap_or_else(|| s.names.clone()).into_iter().collect();
    let mut waiting: BTreeMap<String, String> = BTreeMap::new();
    tokio::time::sleep(Duration::from_secs(5)).await;
    while !peer.is_closed() {
        let now = Timestamp::now();
        let mut problem: Option<String> = None;
        for target in &s.to {
            let d = state.get(target).cloned().unwrap_or_default();
            let wanted = plugin.wanted.lock().await.get(&(target.clone(), s.set.clone())).cloned();
            let Some(reason) = due(&d, wanted.as_ref(), s.refresh_days, now) else {
                waiting.remove(target);
                continue;
            };
            let why = plain_line(&format!(
                "read {} for {} from Chrome on {} and send them to {target}, for {reason}",
                s.names.join(", "),
                s.domains.join(", "),
                plugin.node
            ));
            match approved(&peer, &plugin.node, &s, target, &why).await {
                Ok(false) => {
                    waiting.insert(target.clone(), "the person's approval".into());
                    continue;
                }
                Err(e) if e.starts_with("asking the control plane") => {
                    // Not an answer: try again next round, and say so.
                    problem = Some(format!("cookies {}: {e}", s.set));
                    continue;
                }
                Err(e) => {
                    // Refused: counted as answered, so it isn't asked again until the target asks again or the
                    // refresh comes round.
                    tracing::warn!(set = %s.set, target, "not reading the browser: {e}");
                    waiting.insert(target.clone(), format!("refused: {e}"));
                    let entry = state.entry(target.clone()).or_default();
                    entry.answered = wanted.as_ref().map(|w| w.0).or(Some(now));
                    entry.delivered_at.get_or_insert(now);
                    continue;
                }
                Ok(true) => {}
            }
            waiting.remove(target);
            let read = browser::read(&s.playwright, &s.profile, &s.domains).await;
            let mut cookies = match read {
                Ok(c) => c,
                Err(e) => {
                    problem = Some(format!("cookies {}: reading Chrome: {e}", s.set));
                    continue;
                }
            };
            cookies.retain(|c| s.names.contains(&c.name));
            let missing: Vec<&String> = required.iter().filter(|n| !cookies.iter().any(|c| &c.name == *n)).collect();
            if !missing.is_empty() {
                let missing: Vec<&str> = missing.iter().map(|n| n.as_str()).collect();
                problem = Some(format!(
                    "cookies {}: Chrome has no {} cookie for {}; sign in there in Chrome",
                    s.set,
                    missing.join(" or "),
                    s.domains.join(", ")
                ));
                continue;
            }
            let version = now.as_millisecond() as u64;
            let count = cookies.len();
            let body = Body {
                set: s.set.clone(),
                domains: s.domains.clone(),
                read_at: now,
                cookies,
                refresh_days: Some(s.refresh_days),
            };
            let publish = SyncPublish {
                bundle: format!("cookies/{}", s.set),
                version,
                secret: true,
                targets: vec![ext::SyncTarget { node: target.clone() }],
                items: vec![SyncItem {
                    name: ITEM.into(),
                    content: serde_json::to_string(&body).unwrap_or_default(),
                    mode: Some(0o600),
                    link: None,
                }],
            };
            match peer.request_within::<_, Value>(ext::method::SYNC_PUBLISH, &publish, Duration::from_secs(90)).await {
                Ok(_) => {
                    tracing::info!(set = %s.set, version, domains = ?s.domains, target, cookies = count, %reason,
                        "read from Chrome and sent");
                    let entry = state.entry(target.clone()).or_default();
                    entry.delivered_at = Some(now);
                    entry.answered = wanted.as_ref().map(|w| w.0).or(entry.answered);
                }
                Err(e) => problem = Some(format!("cookies {}: handing the bundle to the node: {e}", s.set)),
            }
        }
        if let Err(e) = write::atomic(&state_path, &serde_json::to_string_pretty(&state).unwrap_or_default()) {
            tracing::warn!("saving {}: {e}", state_path.display());
        }
        if let Some(p) = &problem {
            tracing::warn!("{p}");
        }
        let targets: BTreeMap<&String, Value> =
            s.to.iter()
                .map(|t| {
                    let d = state.get(t).cloned().unwrap_or_default();
                    (t, json!({"deliveredAt": d.delivered_at.map(|x| x.to_string()), "waitingFor": waiting.get(t)}))
                })
                .collect();
        let data = json!({"set": s.set, "source": "browser", "domains": s.domains, "names": s.names, "to": s.to,
            "targets": targets, "readAt": now.to_string(), "problem": problem});
        plugin.share_problems.lock().await.insert(s.set.clone(), problem);
        let reading = ext::Reading { kind: "cookies.share".into(), key: s.set.clone(), at: now, node: None, data };
        peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: vec![reading] });
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// `fleetlayer-cookies request <set> [--why "<reason>"]`, on a target: ask the source for a fresh read. The next
/// `cookies.receive` reading carries it; the source asks the person to approve the read.
fn request_command(args: &[String]) -> Result<String, String> {
    let set = args
        .first()
        .filter(|s| set_name_ok(s))
        .ok_or("usage: fleetlayer-cookies request <set> [--why \"<reason>\"]")?;
    let why = match args.get(1).map(String::as_str) {
        Some("--why") => args.get(2).cloned().ok_or("--why needs a reason")?,
        Some(other) => return Err(format!("unknown argument {other}")),
        None => String::new(),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME isn't set")?;
    let req = Request { at: Timestamp::now(), why: plain_line(&why) };
    let path = request_file(&home, set);
    write::atomic(&path, &serde_json::to_string(&req).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "Asked for a fresh read of the {set} cookies. The machine with the browser asks the person to approve it; the \
         files are rewritten when it arrives (check with: fleetlayer readings cookies.receive)."
    ))
}

/// Every 30 seconds: the plugin's status (every set's problem), and a `cookies.receive` reading per received set.
async fn status_loop(plugin: Arc<Plugin>, peer: Peer) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    while !peer.is_closed() {
        let now = Timestamp::now();
        let mut problems: Vec<String> = plugin.share_problems.lock().await.values().flatten().cloned().collect();
        let mut readings = vec![];
        let rec = plugin.received.lock().await;
        for r in &plugin.receive {
            let st = rec.get(&r.set).cloned().unwrap_or_default();
            let mut problem = receive_problem(r, &st, now, plugin.started);
            if let Some((e, at)) = plugin.refused.lock().await.get(&r.set).cloned()
                && now.duration_since(at).unsigned_abs() < Duration::from_secs(24 * 3600)
            {
                problem =
                    Some(format!("cookies: {e} (at {}); another node tried to send this set", when(at.as_second())));
            }
            let (earliest, _, _) = check(&st.cookies, &r.required(), now.as_second());
            // A request made with `fleetlayer-cookies request`, until a version arrives after it.
            let request = read_request(&plugin.home, &r.set).filter(|q| st.applied_at.is_none_or(|a| a < q.at));
            readings.push(ext::Reading {
                kind: "cookies.receive".into(),
                key: r.set.clone(),
                at: now,
                node: None,
                data: json!({"set": r.set, "version": st.version, "appliedAt": st.applied_at.map(|t| t.to_string()),
                    "readAt": st.read_at.map(|t| t.to_string()), "cookies": st.cookies.len(),
                    "earliestExpiry": earliest.map(when), "files": st.written, "problem": problem,
                    "requestedAt": request.as_ref().map(|q| q.at.to_string()),
                    "requestWhy": request.as_ref().map(|q| q.why.clone())}),
            });
            problems.extend(problem);
        }
        drop(rec);
        if !readings.is_empty() {
            peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings });
        }
        let report = ext::PluginStatusReport {
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
            detail: json!({"problems": problems.len()}),
        };
        peer.notify(ext::method::PLUGIN_STATUS, &report);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn main() {
    // Before any thread starts: on macOS, start again as its own responsible process (src/disclaim.rs).
    disclaim::relaunch();
    // `fleetlayer-cookies responsible`: whether macOS holds this program itself responsible for what it reads, after
    // that relaunch, for checking a setup (docs/plugins/cookies.md).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("request") {
        match request_command(&args[1..]) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("fleetlayer-cookies: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("responsible") {
        let me = std::process::id() as i32;
        match disclaim::responsible_pid() {
            Some(pid) if pid == me => println!("fleetlayer-cookies (pid {me}) is its own responsible process"),
            Some(pid) => println!("pid {pid} is responsible for fleetlayer-cookies (pid {me})"),
            None => println!("this system doesn't say which process is responsible (only macOS does)"),
        }
        return;
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a Tokio runtime").block_on(serve());
}

async fn serve() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Mutex::new(None)))).await;
}


/// The key that signed a delivered bundle, if it's signed (`fleetlayer_secrets::sign`); an error when its signature
/// doesn't hold.
fn signer_of(a: &SyncApply) -> Result<Option<String>, String> {
    let parts: Vec<fleetlayer_secrets::sign::Part> = a
        .items
        .iter()
        .map(|i| fleetlayer_secrets::sign::Part {
            name: &i.name,
            content: &i.content,
            mode: i.mode,
            link: i.link.as_deref(),
        })
        .collect();
    fleetlayer_secrets::sign::verify(&a.bundle, a.version, &parts)
}

#[cfg(test)]
mod tests;
