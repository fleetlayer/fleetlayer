//! fleetlayer-signin: one sign-in, with passkeys, for every web service a fleet runs. An OpenID Connect provider:
//! services (the hub, pages, any OIDC client) send people here, and get a token for one tenant once the tenant's
//! policy allows `sign_in` on that service. It holds every credential (passkeys, sessions, the token-signing key) in
//! one folder; who belongs where is the control plane's, asked with the sign-in token. `docs/sso.md` is the design.
//!
//! ```yaml
//! - name: signin
//!   command: fleetlayer-signin
//!   config:
//!     dir: ~/.local/state/fleetlayer/signin
//!     issuer: https://signin.example.com
//!     control_plane: https://control.example.com
//!     token_file: ~/.config/fleetlayer/signin-token
//!     listen: 127.0.0.1:7680
//!     clients:
//!       - name: hub
//!         redirect_uris: [https://hub.example.com/auth/callback]
//!         secret_file: ~/.config/fleetlayer/signin-clients/hub
//! ```
//!
//! `fleetlayer-signin serve <config.json>` runs the same service on its own (the plugin config as JSON).

mod admin;
mod control;
mod domain;
mod forward;
mod google;
mod keys;
mod logout;
mod store;
mod teams;
mod upstream;
mod web;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, NodeMeta, PluginMeta, Roles, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The team every member is in; pages leave it out until there's a second.
pub const EVERYONE: &str = "everyone";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientConfig {
    name: String,
    redirect_uris: Vec<String>,
    /// A confidential client's secret; none for a public client (PKCE is required of both).
    #[serde(default)]
    secret_file: Option<PathBuf>,
    #[serde(default)]
    post_logout_redirect_uris: Vec<String>,
    /// Where it takes OpenID Connect back-channel logouts, so signing out here signs out there too.
    #[serde(default)]
    backchannel_logout_uri: Option<String>,
}

/// A provider people may sign in with (`docs/sso.md`, "Upstream providers").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamConfig {
    name: String,
    /// `google` (Google Workspace: its `hd` and a verified email checked against `domain`) or `oidc`.
    kind: String,
    /// The provider (an `oidc` one; Google's is known).
    #[serde(default)]
    issuer: Option<String>,
    /// For Google Workspace, the domain (required); for `oidc`, optional, checked the same way.
    #[serde(default)]
    domain: Option<String>,
    /// What the button says, after "Continue with" (default: Google, or the name).
    #[serde(default)]
    label: Option<String>,
    /// The tenant it signs people in for (slug or ID).
    tenant: String,
    client_id: String,
    client_secret_file: PathBuf,
    /// `{role: member}`: anyone it vouches for may join the tenant as a member.
    #[serde(default)]
    join: Option<JoinConfig>,
    /// Google Workspace: read people's groups with a service account, and map some to teams.
    #[serde(default)]
    teams: Option<GoogleTeamsConfig>,
    /// An OIDC provider: the ID token claim that lists someone's groups (default `groups`), with `map`.
    #[serde(default)]
    teams_claim: Option<String>,
    /// An OIDC provider: group to team (or teams).
    #[serde(default)]
    map: BTreeMap<String, OneOrMany>,
    /// Google: how long people keep the teams from the last good read while Google can't be read (default 24).
    #[serde(default)]
    teams_grace_hours: Option<i64>,
    /// OIDC: how long teams last without a sign-in through it (default 30).
    #[serde(default)]
    teams_expire_days: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoogleTeamsConfig {
    /// The service account's JSON key, readable by this user only.
    service_account_file: PathBuf,
    /// An admin to act as, with domain-wide delegation; leave out when the service account has an admin role.
    #[serde(default)]
    subject: Option<String>,
    /// Group email to team (or teams).
    map: BTreeMap<String, OneOrMany>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

fn team_map(map: BTreeMap<String, OneOrMany>, upstream: &str) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    for (group, teams) in map {
        let teams = match teams {
            OneOrMany::One(t) => vec![t],
            OneOrMany::Many(ts) => ts,
        };
        for t in &teams {
            anyhow::ensure!(
                !t.is_empty()
                    && t.chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '-'
                        || c == '_'
                        || c == '.'),
                "upstream {upstream}: {t:?} isn't a team name"
            );
        }
        out.insert(group.to_ascii_lowercase(), teams);
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinConfig {
    role: String,
    /// Required, and true, on an upstream with no `domain`: anyone that provider vouches for may join. On a public
    /// provider (GitLab.com, a social sign-in) that's anyone on the internet, so it's never the default.
    #[serde(default)]
    anyone_it_vouches_for: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// All its state: people.json, keys/, sessions.json.
    dir: PathBuf,
    /// The address services send people to (`https://signin.example.com`).
    issuer: String,
    /// What passkeys are made for; by default the issuer's registrable domain.
    #[serde(default)]
    passkey_domain: Option<String>,
    /// Addresses it had before, still pointed here while every service is repointed (`docs/sso.md`, "Moving it").
    #[serde(default)]
    previous_issuers: Vec<String>,
    /// Forward-auth for services that can't speak OpenID Connect (`docs/plugins/signin.md`).
    #[serde(default)]
    forward_auth: Option<ForwardConfig>,
    control_plane: String,
    /// The sign-in token (`fleetlayer-control signin token`).
    token_file: PathBuf,
    listen: SocketAddr,
    #[serde(default)]
    clients: Vec<ClientConfig>,
    #[serde(default)]
    upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    heartbeat: Option<Heartbeat>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardConfig {
    /// The parent domain the cookie is set on; the sign-in service and every host must be under it.
    cookie_domain: String,
    hosts: Vec<ForwardHostConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardHostConfig {
    host: String,
    service: String,
    tenant: String,
}

/// Whether `host` (no port) is `domain` or under it.
fn under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn forward_settings(f: ForwardConfig, issuer: &url::Url) -> anyhow::Result<forward::Settings> {
    let domain = f.cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    anyhow::ensure!(
        domain.contains('.') && !domain.contains(['/', ':', ' ']),
        "forward_auth.cookie_domain {:?}: a domain such as example.com",
        f.cookie_domain
    );
    let own = issuer.host_str().unwrap_or_default().to_ascii_lowercase();
    anyhow::ensure!(
        under(&own, &domain),
        "forward_auth.cookie_domain {domain}: the sign-in service ({own}) must be under it, to set its cookie"
    );
    let mut hosts = vec![];
    for h in f.hosts {
        anyhow::ensure!(valid_client_name(&h.service), "forward_auth host {}: service {:?}", h.host, h.service);
        let host = h.host.to_ascii_lowercase();
        let bare = host.rsplit_once(':').map(|(b, _)| b).unwrap_or(&host);
        anyhow::ensure!(
            !host.contains(['/', ' ', '@']) && under(bare, &domain),
            "forward_auth host {host}: a host name under {domain}, which the cookie reaches"
        );
        hosts.push(forward::Host { host, service: h.service, tenant: h.tenant });
    }
    Ok(forward::Settings { cookie_domain: domain, hosts })
}

/// Pushed every five minutes (Gatus external-endpoint style: `?success=true|false&error=…`, with a bearer token),
/// failing when the folder can't be written or the control plane doesn't answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    url: String,
    token_file: PathBuf,
}

fn expand(home: &Path, p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn read_secret(path: &Path, what: &str) -> anyhow::Result<String> {
    let s = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {what} {}: {e}", path.display()))?;
    let s = s.trim().to_string();
    anyhow::ensure!(!s.is_empty(), "{what} {} is empty", path.display());
    Ok(s)
}

fn valid_client_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn build(cfg: Config, home: &Path) -> anyhow::Result<(Arc<web::App>, SocketAddr, Option<Heartbeat>)> {
    let issuer = url::Url::parse(&cfg.issuer).map_err(|e| anyhow::anyhow!("issuer {}: {e}", cfg.issuer))?;
    anyhow::ensure!(
        issuer.scheme() == "https" || (issuer.scheme() == "http" && domain::local(&issuer)),
        "the issuer must be https (plain http only on this machine: localhost or *.localhost)"
    );
    anyhow::ensure!(
        issuer.query().is_none() && issuer.fragment().is_none(),
        "the issuer can't have a query or fragment"
    );
    let (passkey_domain, passkeys_fixed) = domain::passkey_domain(&issuer, cfg.passkey_domain.as_deref())?;
    let mut clients = vec![];
    for c in cfg.clients {
        anyhow::ensure!(valid_client_name(&c.name), "client {:?}: lower-case letters, digits and hyphens", c.name);
        anyhow::ensure!(!c.redirect_uris.is_empty(), "client {} has no redirect_uris", c.name);
        for u in c.redirect_uris.iter().chain(&c.post_logout_redirect_uris).chain(&c.backchannel_logout_uri) {
            let parsed = url::Url::parse(u).map_err(|e| anyhow::anyhow!("client {}: {u}: {e}", c.name))?;
            anyhow::ensure!(
                parsed.scheme() == "https" || (parsed.scheme() == "http" && domain::local(&parsed)),
                "client {}: {u} must be https (plain http only on this machine)",
                c.name
            );
            anyhow::ensure!(parsed.fragment().is_none(), "client {}: {u} can't have a fragment", c.name);
        }
        let secret = match &c.secret_file {
            Some(f) => Some(read_secret(&expand(home, f), &format!("client {}'s secret", c.name))?),
            None => None,
        };
        clients.push(web::Client {
            name: c.name,
            redirect_uris: c.redirect_uris,
            secret,
            post_logout_redirect_uris: c.post_logout_redirect_uris,
            backchannel_logout_uri: c.backchannel_logout_uri,
        });
    }
    let dir = expand(home, &cfg.dir);
    let store = store::Store::open(&dir)?;
    let keys = keys::Keys::load_or_make(&dir)?;
    let token = read_secret(&expand(home, &cfg.token_file), "the sign-in token")?;
    let control = control::Control::new(&cfg.control_plane, &token);
    let host_port = |u: &url::Url| {
        u.host_str().map(|h| match u.port() {
            Some(port) => format!("{h}:{port}").to_ascii_lowercase(),
            None => h.to_ascii_lowercase(),
        })
    };
    let mut previous_issuers = vec![];
    for p in &cfg.previous_issuers {
        let u = url::Url::parse(p).map_err(|e| anyhow::anyhow!("previous_issuers: {p}: {e}"))?;
        let host = host_port(&u).ok_or_else(|| anyhow::anyhow!("previous_issuers: {p} has no host"))?;
        anyhow::ensure!(
            Some(&host) != host_port(&issuer).as_ref(),
            "previous_issuers: {p} is the issuer's own address"
        );
        previous_issuers.push(host);
    }
    let forward = cfg.forward_auth.map(|f| forward_settings(f, &issuer)).transpose()?;
    let mut origin = issuer.clone();
    origin.set_path("");
    let settings = web::Settings {
        issuer: cfg.issuer.trim_end_matches('/').to_string(),
        origin,
        passkey_domain,
        passkeys_fixed,
        clients,
        previous_issuers,
        forward,
    };
    let mut upstreams = vec![];
    for u in cfg.upstreams {
        anyhow::ensure!(valid_client_name(&u.name), "upstream {:?}: lower-case letters, digits and hyphens", u.name);
        let (issuer, label) = match u.kind.as_str() {
            "google" => {
                anyhow::ensure!(
                    u.domain.is_some(),
                    "upstream {}: a google upstream needs its Workspace domain",
                    u.name
                );
                ("https://accounts.google.com".to_string(), u.label.clone().unwrap_or_else(|| "Google".into()))
            }
            "oidc" => (
                u.issuer
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("upstream {}: an oidc upstream needs its issuer", u.name))?,
                u.label.clone().unwrap_or_else(|| u.name.clone()),
            ),
            k => anyhow::bail!("upstream {}: kind is google or oidc (got {k})", u.name),
        };
        if let Some(j) = &u.join {
            anyhow::ensure!(j.role == "member", "upstream {}: join can only make members", u.name);
            anyhow::ensure!(
                u.domain.is_some() || j.anyone_it_vouches_for,
                "upstream {}: join with no domain lets anyone {} vouches for into {} (on a public provider, anyone on \
                 the internet). Set domain, or say it outright with join: {{role: member, anyone_it_vouches_for: true}}",
                u.name,
                issuer,
                u.tenant
            );
        }
        let day = 86_400_000;
        let teams = match (u.kind.as_str(), u.teams, u.teams_claim, u.map.is_empty()) {
            ("google", Some(g), None, true) => Some(teams::Teams {
                source: teams::Source::Google(Box::new(google::Directory::from_file(
                    &expand(home, &g.service_account_file),
                    g.subject,
                )?)),
                map: team_map(g.map, &u.name)?,
                grace_ms: u.teams_grace_hours.unwrap_or(24) * 3_600_000,
                expire_ms: 0,
            }),
            ("oidc", None, claim, false) => Some(teams::Teams {
                source: teams::Source::Claim(claim.unwrap_or_else(|| "groups".into())),
                map: team_map(u.map, &u.name)?,
                grace_ms: 0,
                expire_ms: u.teams_expire_days.unwrap_or(30) * day,
            }),
            (_, None, None, true) => None,
            ("google", ..) => anyhow::bail!(
                "upstream {}: for Google Workspace, groups map to teams under teams: {{service_account_file, map}}",
                u.name
            ),
            _ => anyhow::bail!("upstream {}: for an oidc provider, set map (and teams_claim, if not groups)", u.name),
        };
        let secret =
            read_secret(&expand(home, &u.client_secret_file), &format!("upstream {}'s client secret", u.name))?;
        let back = format!("{}/upstream/{}/callback", settings.issuer, u.name);
        let oidc = fleetlayer_oidc::Oidc::with_redirect(&issuer, &u.client_id, &secret, &back, &settings.issuer)?;
        upstreams.push(upstream::Upstream {
            name: u.name,
            label,
            domain: u.domain.map(|d| d.to_ascii_lowercase()),
            hd: u.kind == "google",
            tenant: u.tenant,
            join: u.join.is_some(),
            oidc,
            teams,
        });
    }
    let app = web::App::with_upstreams(settings, store, keys, control, upstreams)?;
    let heartbeat = cfg.heartbeat.map(|h| Heartbeat { token_file: expand(home, &h.token_file), ..h });
    Ok((Arc::new(app), cfg.listen, heartbeat))
}

async fn beat(hb: &Heartbeat, problems: &[String]) {
    let token = match std::fs::read_to_string(&hb.token_file) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("reading the heartbeat token {}: {e}", hb.token_file.display());
            return;
        }
    };
    let mut url = format!("{}{}success={}", hb.url, if hb.url.contains('?') { '&' } else { '?' }, problems.is_empty());
    if !problems.is_empty() {
        url.push_str("&error=");
        url.push_str(&form_urlencoded::byte_serialize(problems.join("; ").as_bytes()).collect::<String>());
    }
    let res = reqwest::Client::new().post(&url).bearer_auth(token.trim()).timeout(Duration::from_secs(20)).send().await;
    if let Err(e) = res.and_then(|r| r.error_for_status()) {
        tracing::warn!("the heartbeat: {e}");
    }
}

fn loopback_if_any(a: SocketAddr) -> SocketAddr {
    if a.ip().is_unspecified() { SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port()) } else { a }
}

async fn run(
    app: Arc<web::App>,
    listen: SocketAddr,
    heartbeat: Option<Heartbeat>,
    peer: Option<Peer>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = loopback_if_any(listener.local_addr()?);
    tracing::info!(%bound, issuer = %app.settings.issuer, passkeys_for = %app.settings.passkey_domain, "sign-in listening");
    if let Some(peer) = peer {
        peer.notify(
            ext::method::SERVICE_LISTENING,
            &ext::ServiceListening {
                name: "signin".into(),
                url: format!("http://{bound}"),
                health: Some("/healthz".into()),
                hosts: vec![],
            },
        );
    }
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let k = a.clone();
            let _ = tokio::task::spawn_blocking(move || k.refresh_keys()).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = a.apply_revocations().await {
                tracing::warn!("signing people out from revocations.json: {e:#}");
            }
            a.deliver_logouts().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        // Upstream groups read again daily (hourly while failing); the first pass a minute after start.
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            a.resync_teams(false).await;
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let problems = a.problems().await;
            for p in &problems {
                tracing::warn!("{p}");
            }
            if let Some(hb) = &heartbeat {
                beat(hb, &problems).await;
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
    axum::serve(listener, web::router(app)).await?;
    Ok(())
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-signin runs under a Fleetlayer node"))?;
                let cfg: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let (app, listen, heartbeat) =
                    build(cfg, Path::new(&node.home)).map_err(|e| RpcError::refused(e.to_string()))?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    let peer = peer.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run(app, listen, heartbeat, Some(peer)).await {
                            // Exiting makes the node restart the plugin, with backoff, and report it.
                            tracing::error!("the sign-in service stopped: {e}");
                            std::process::exit(1);
                        }
                    });
                }
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(
                            meta_key::PLUGIN,
                            &PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() },
                        )),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-signin".into(),
                        title: Some("Sign-in".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

const KEYS_USAGE: &str = "usage: fleetlayer-signin keys rotate --dir <folder> [--drop-previous]";

/// `keys rotate --dir <folder> [--drop-previous]`: a new signing key now. The running service picks it up within a
/// minute. `--drop-previous` is for a key that leaked: the old key leaves the JWKS at once, so every token it signed
/// stops verifying and people sign in to services again.
fn keys_command(args: &[&str]) -> anyhow::Result<()> {
    let mut dir = None;
    let mut drop_previous = false;
    let mut it = args.iter();
    anyhow::ensure!(it.next() == Some(&"rotate"), "{KEYS_USAGE}");
    while let Some(a) = it.next() {
        match *a {
            "--dir" => dir = it.next().map(PathBuf::from),
            "--drop-previous" => drop_previous = true,
            other => anyhow::bail!("unknown argument {other:?}\n{KEYS_USAGE}"),
        }
    }
    let dir = dir.ok_or_else(|| anyhow::anyhow!("{KEYS_USAGE}"))?;
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let dir = expand(&home, &dir);
    anyhow::ensure!(dir.join("keys").join("signing.pem").exists(), "no signing key in {}", dir.display());
    let kid = keys::Keys::rotate_files(&dir, store::now_ms(), drop_previous)?;
    println!("new signing key {kid}");
    if drop_previous {
        println!("the previous key is gone from the JWKS: tokens it signed no longer verify");
    } else {
        println!("the previous key stays in the JWKS for two weeks");
    }
    println!("the running service uses it within a minute");
    Ok(())
}

const SIGNOUT_USAGE: &str = "usage: fleetlayer-signin signout --dir <folder> --person <id>";

/// `signout --dir <folder> --person <id>`: sign someone out everywhere (an account that may be in someone else's
/// hands). The running service ends their sessions and sends the logouts within seconds.
fn signout_command(args: &[&str]) -> anyhow::Result<()> {
    let (mut dir, mut person) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--dir" => dir = it.next().map(PathBuf::from),
            "--person" => person = it.next().map(|p| p.to_string()),
            other => anyhow::bail!("unknown argument {other:?}\n{SIGNOUT_USAGE}"),
        }
    }
    let (Some(dir), Some(person)) = (dir, person) else { anyhow::bail!("{SIGNOUT_USAGE}") };
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let dir = expand(&home, &dir);
    anyhow::ensure!(dir.join("keys").exists(), "{} isn't a sign-in folder", dir.display());
    logout::revoke(&dir, &person, store::now_ms())?;
    println!("{person} is signed out everywhere within seconds, and of every service that takes sign-outs");
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: fleetlayer-signin serve <config.json>");
                std::process::exit(2);
            };
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
            let result = async {
                let cfg: Config = serde_json::from_slice(&std::fs::read(path)?)?;
                let (app, listen, heartbeat) = build(cfg, &home)?;
                run(app, listen, heartbeat, None).await
            }
            .await;
            if let Err(e) = result {
                eprintln!("fleetlayer-signin: {e}");
                std::process::exit(1);
            }
        }
        Some("keys") => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if let Err(e) = keys_command(&rest) {
                eprintln!("fleetlayer-signin: {e:#}");
                std::process::exit(1);
            }
        }
        Some("signout") => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if let Err(e) = signout_command(&rest) {
                eprintln!("fleetlayer-signin: {e:#}");
                std::process::exit(1);
            }
        }
        Some("--version") => println!("fleetlayer-signin {}", env!("CARGO_PKG_VERSION")),
        Some(other) => {
            eprintln!(
                "fleetlayer-signin: unknown command {other:?} (run by a Fleetlayer node, or `serve <config.json>`)"
            );
            std::process::exit(2);
        }
        None => fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await,
    }
}
