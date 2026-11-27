//! fleetlayer-1password: deliver 1Password items to the roles granted them. The source runs under a Unix user of its
//! own, holding the service-account token(s), so no agent holds a token that opens a whole vault. Grants name items
//! (`op://vault/item/field`) and the nodes of a role; each node gets one bundle, `1password/<node>`, sealed to its key,
//! holding exactly the items granted to it, and again whenever one changes. On each target the plugin lists them in
//! the node's secrets registry (for `fleetlayer secrets run` and scrubbing) and writes any env files or files its
//! config asks for. `docs/plugins/1password.md` has the whole story.
//!
//! ```yaml
//! - name: 1password
//!   command: fleetlayer-1password
//!   grants: [sync.publish, reading.publish]
//!   config:
//!     source:                                   # on the 1Password user's node
//!       token_files: [~/.config/fleetlayer/1password/token]
//!       roles:
//!         - role: everyday
//!           nodes: [agents, mac-agents]
//!           read: [op://github/deploy-bot/token]
//!     receive:                                  # on each role's node
//!       write:
//!         - { format: env, path: ~/.config/fleetlayer/secrets/github.env, values: { GITHUB_TOKEN: op://github/deploy-bot/token } }
//! ```

mod op;

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
use fleetlayer_secrets::{Secret, registry, write};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use op::{Op, OpError, Reference};

const ITEM: &str = "secrets.json";
/// The first line of every env file this plugin writes, which marks it as the plugin's.
const ENV_HEADER: &str = "# Written by fleetlayer-1password, and replaced whenever an item changes.\n";
const BUNDLE_PREFIX: &str = "1password/";
/// The registry's source name on targets.
const SOURCE: &str = "1password";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Role {
    /// A name for people and readings (`everyday`, `infrastructure`).
    role: String,
    /// The nodes (Unix users) in this role: one, or the same role on several machines.
    nodes: Vec<String>,
    /// The items this role may read.
    read: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    /// Service-account token files (0600), one per 1Password account. Each reference is read with the token whose
    /// account has its vault.
    token_files: Vec<PathBuf>,
    roles: Vec<Role>,
    /// `op` (1Password's command line), when it isn't on PATH.
    #[serde(default = "default_op")]
    op: String,
    #[serde(default = "five")]
    every_minutes: u64,
    /// Send again after this long even when nothing changed, so a target can tell the source is alive.
    #[serde(default = "twelve")]
    resend_hours: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    Env,
    File,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    format: Format,
    path: PathBuf,
    /// For `env`: variable name → reference.
    #[serde(default)]
    values: BTreeMap<String, String>,
    /// For `file`: the one reference whose value is the file.
    #[serde(default, rename = "ref")]
    reference: Option<String>,
}

impl Output {
    fn references(&self) -> Vec<&str> {
        match self.format {
            Format::Env => self.values.values().map(String::as_str).collect(),
            Format::File => self.reference.as_deref().into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receive {
    /// The source's node: bundles from any other node are refused, since any node can run a plugin called 1password
    /// and publish a bundle named after this node.
    from: String,
    #[serde(default)]
    write: Vec<Output>,
    /// Report a problem when nothing has arrived for this long.
    #[serde(default = "forty_eight")]
    stale_hours: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    source: Option<Source>,
    #[serde(default)]
    receive: Option<Receive>,
}

fn default_op() -> String {
    "op".into()
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

fn expand(home: &Path, p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// The config with `~` expanded, or why it's wrong.
fn load(home: &Path, raw: &Value) -> Result<Config, String> {
    let mut c: Config = serde_json::from_value(raw.clone()).map_err(|e| format!("fleetlayer-1password config: {e}"))?;
    if let Some(s) = &mut c.source {
        if s.token_files.is_empty() {
            return Err("source: token_files names no service-account token".into());
        }
        for t in &mut s.token_files {
            *t = expand(home, t);
        }
        let mut roles = BTreeSet::new();
        for r in &s.roles {
            if !name_ok(&r.role) || !roles.insert(r.role.clone()) {
                return Err(format!("source: {:?} isn't a unique role name", r.role));
            }
            if r.nodes.is_empty() || !r.nodes.iter().all(|n| name_ok(n)) {
                return Err(format!("source role {}: nodes must name at least one node", r.role));
            }
            for x in &r.read {
                Reference::parse(x).map_err(|e| format!("source role {}: {e}", r.role))?;
            }
        }
    }
    if let Some(rc) = &mut c.receive {
        if !name_ok(&rc.from) {
            return Err("receive: from must name the 1Password source's node".into());
        }
        let mut paths = BTreeSet::new();
        for o in &mut rc.write {
            o.path = expand(home, &o.path);
            if !o.path.is_absolute() || !paths.insert(o.path.clone()) {
                return Err(format!("receive: {} must be absolute and written once", o.path.display()));
            }
            match o.format {
                Format::Env if o.values.is_empty() || !o.values.keys().all(|k| write::env_var_ok(k)) => {
                    return Err(format!(
                        "receive {}: an env file needs values, variable names to references \
                         ({{GITHUB_TOKEN: op://github/bot/token}})",
                        o.path.display()
                    ));
                }
                Format::Env if o.reference.is_some() => {
                    return Err(format!("receive {}: ref is for the file format", o.path.display()));
                }
                Format::File if o.reference.is_none() || !o.values.is_empty() => {
                    return Err(format!("receive {}: a file takes one ref and no values", o.path.display()));
                }
                _ => {}
            }
            for r in o.references() {
                Reference::parse(r).map_err(|e| format!("receive {}: {e}", o.path.display()))?;
            }
        }
    }
    Ok(c)
}

/// What a bundle carries: one JSON item.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    node: String,
    read_at: Timestamp,
    secrets: Vec<Delivered>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivered {
    #[serde(rename = "ref")]
    reference: String,
    value: String,
    /// When 1Password last changed the item.
    #[serde(default)]
    updated_at: Option<String>,
}

/// Each node's references, from every role that names it.
fn grants_by_node(roles: &[Role]) -> BTreeMap<String, BTreeSet<String>> {
    let mut by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in roles {
        for n in &r.nodes {
            by.entry(n.clone()).or_default().extend(r.read.iter().cloned());
        }
    }
    by
}

/// What the source knows of an item it read: never written to disk.
#[derive(Clone)]
struct Known {
    item_id: String,
    version: u64,
    updated_at: Option<String>,
    value: String,
}

/// The source's memory between rounds.
#[derive(Default)]
struct SourceState {
    /// Vault name or ID → (token index, vault ID).
    vaults: HashMap<String, (usize, String)>,
    /// Vault names more than one account has: references to them are ambiguous.
    clashes: BTreeSet<String>,
    vaults_at: Option<std::time::Instant>,
    known: HashMap<String, Known>,
}

/// One round's result. Each reference ends up delivered (read now, or its last value when it couldn't be read this
/// time), gone (1Password says it isn't there: left out, so targets remove it), or stuck (it couldn't be read and has
/// no value from before: a node granted it is held back rather than sent a bundle without it).
#[derive(Default)]
struct Round {
    values: BTreeMap<String, Delivered>,
    /// References 1Password says aren't there (removed or renamed), and why.
    missing: Vec<(String, String)>,
    stuck: BTreeSet<String>,
    /// For the plugin's status: what failed, by reference or token. Never values.
    problems: Vec<String>,
}

impl Round {
    /// `r` couldn't be read now: deliver its last value if there is one, else hold back the nodes granted it.
    fn stuck(&mut self, st: &SourceState, r: &str, why: String) {
        match st.known.get(r) {
            Some(k) => {
                self.values.insert(
                    r.to_string(),
                    Delivered { reference: r.to_string(), value: k.value.clone(), updated_at: k.updated_at.clone() },
                );
                self.problems.push(format!("{r}: {why} (still sending its last value)"));
            }
            None => {
                self.stuck.insert(r.to_string());
                self.problems.push(format!("{r}: {why}"));
            }
        }
    }
}

/// Read every reference. `tokens` has one entry per token file, None for one that couldn't be read.
async fn collect(op: &Op, tokens: &[Option<String>], refs: &BTreeSet<String>, st: &mut SourceState) -> Round {
    let mut round = Round::default();
    let parsed: Vec<Reference> = refs.iter().filter_map(|r| Reference::parse(r).ok()).collect();
    // Which token sees which vault: asked again hourly, and whenever a vault isn't known. A token that can't list its
    // vaults keeps what it listed before.
    let unknown = parsed.iter().any(|r| !st.vaults.contains_key(&r.vault));
    let mut all_listed = tokens.iter().all(Option::is_some);
    if st.vaults_at.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) || unknown {
        let mut listed: Vec<Option<Vec<op::Vault>>> = vec![];
        for (i, t) in tokens.iter().enumerate() {
            let Some(t) = t else {
                listed.push(None);
                continue;
            };
            match op.vaults(t).await {
                Ok(vs) => listed.push(Some(vs)),
                Err(e) => {
                    round.problems.push(format!("token {}: {e}", i + 1));
                    all_listed = false;
                    listed.push(None);
                }
            }
        }
        let mut vaults: HashMap<String, (usize, String)> = st
            .vaults
            .iter()
            .filter(|(_, (i, _))| listed.get(*i).is_some_and(Option::is_none))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut clashes = BTreeSet::new();
        for (i, vs) in listed.iter().enumerate() {
            for v in vs.iter().flatten() {
                for key in [v.name.clone(), v.id.clone()] {
                    if let Some((other, _)) = vaults.insert(key.clone(), (i, v.id.clone()))
                        && other != i
                    {
                        clashes.insert(key);
                    }
                }
            }
        }
        st.vaults = vaults;
        st.clashes = clashes;
        if all_listed {
            st.vaults_at = Some(std::time::Instant::now());
        }
    }
    let mut by_vault: BTreeMap<String, Vec<&Reference>> = BTreeMap::new();
    for r in &parsed {
        by_vault.entry(r.vault.clone()).or_default().push(r);
    }
    for (vault, refs) in by_vault {
        if st.clashes.contains(&vault) {
            for r in refs {
                round.stuck(st, &r.full, format!("two accounts have a vault called {vault:?}; refer to it by its ID"));
            }
            continue;
        }
        let token =
            st.vaults.get(&vault).cloned().and_then(|(ti, id)| tokens.get(ti).cloned().flatten().map(|t| (t, id)));
        let Some((token, vault_id)) = token else {
            for r in refs {
                if st.vaults.contains_key(&vault) || !all_listed {
                    round.stuck(st, &r.full, format!("the token for the {vault:?} vault can't be used now"));
                } else {
                    round
                        .missing
                        .push((r.full.clone(), format!("no account's token can see a vault called {vault:?}")));
                }
            }
            continue;
        };
        let items = match op.items(&token, &vault_id).await {
            Ok(i) => i,
            Err(e) => {
                for r in refs {
                    round.stuck(st, &r.full, e.to_string());
                }
                continue;
            }
        };
        for r in refs {
            let found: Vec<&op::Item> = items.iter().filter(|i| i.id == r.item || i.title == r.item).collect();
            let item = match found.as_slice() {
                [] => {
                    round.missing.push((r.full.clone(), format!("{:?} isn't an item in the {vault:?} vault", r.item)));
                    continue;
                }
                [one] => *one,
                _ => {
                    let why = format!(
                        "the {vault:?} vault has {} items called {:?}; refer to it by its ID",
                        found.len(),
                        r.item
                    );
                    round.stuck(st, &r.full, why);
                    continue;
                }
            };
            // Unchanged only if it's the same item at the same version: a deleted and recreated item is read again.
            let fresh = st
                .known
                .get(&r.full)
                .filter(|k| k.item_id == item.id && k.version == item.version && item.version > 0)
                .cloned();
            let known = match fresh {
                Some(k) => k,
                None => match op.read(&token, &r.full).await {
                    Ok(value) => {
                        let k = Known {
                            item_id: item.id.clone(),
                            version: item.version,
                            updated_at: item.updated_at.clone(),
                            value,
                        };
                        st.known.insert(r.full.clone(), k.clone());
                        k
                    }
                    Err(OpError::NotFound(e)) => {
                        round.missing.push((r.full.clone(), e));
                        continue;
                    }
                    Err(OpError::Failed(e)) => {
                        round.stuck(st, &r.full, e);
                        continue;
                    }
                },
            };
            round.values.insert(
                r.full.clone(),
                Delivered { reference: r.full.clone(), value: known.value, updated_at: known.updated_at },
            );
        }
    }
    let gone: BTreeSet<&String> = round.missing.iter().map(|(r, _)| r).collect();
    st.known.retain(|k, _| refs.contains(k) && !gone.contains(k));
    round
}

/// A fingerprint of what a node would get, for "changed?": references and item versions, never values.
fn digest(secrets: &[Delivered]) -> String {
    let mut h = Sha256::new();
    for s in secrets {
        h.update(s.reference.as_bytes());
        h.update([0]);
        h.update(Sha256::digest(s.value.as_bytes()));
    }
    hex::encode(h.finalize())
}

/// What the source keeps on disk (`~/.local/state/fleetlayer/1password/source.json`): the nodes it delivered to, so a
/// node that loses every grant, even across a restart, gets an empty bundle that removes what it had.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SourceDisk {
    nodes: BTreeSet<String>,
    /// The last bundle version sent. Versions only go up, even when the clock steps back or the source moves to a
    /// machine whose clock is behind (copy this folder when moving it), or targets would ignore what comes next.
    #[serde(default)]
    version: u64,
}

impl SourceDisk {
    fn next_version(&mut self, now: Timestamp) -> u64 {
        self.version = (now.as_millisecond().max(0) as u64).max(self.version + 1);
        self.version
    }
}

/// What a target keeps (`~/.local/state/fleetlayer/1password/received.json`): never values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Received {
    version: u64,
    applied_at: Option<Timestamp>,
    read_at: Option<Timestamp>,
    /// Each reference delivered, and when 1Password last changed its item.
    items: Vec<(String, Option<String>)>,
    /// Files this plugin wrote, so it never overwrites or removes one a person made.
    written: Vec<PathBuf>,
    /// References the config writes that the last bundle didn't have.
    missing: Vec<String>,
    error: Option<String>,
}

fn env_file(values: &BTreeMap<String, String>, got: &BTreeMap<&str, &str>) -> Result<String, String> {
    let mut out = String::from(ENV_HEADER);
    for (var, r) in values {
        let v = got.get(r.as_str()).ok_or_else(|| format!("{r} wasn't delivered"))?;
        if v.contains(['\n', '\r', '\0']) {
            return Err(format!("{r} isn't one line, so it can't go in an env file; write it as a file"));
        }
        out.push_str(&format!("{var}={}\n", write::env_value(v)));
    }
    Ok(out)
}

/// Make the configured files match `secrets`. An output whose references were all delivered is written; one missing
/// any has its file removed (a grant taken away takes the file with it), if this plugin wrote it. Files written
/// before that the config no longer names are removed. Answers what's written now, the references missing, and errors.
fn apply_outputs(
    outputs: &[Output],
    secrets: &[Delivered],
    written_before: &[PathBuf],
) -> (Vec<PathBuf>, Vec<String>, Vec<String>) {
    let got: BTreeMap<&str, &str> = secrets.iter().map(|s| (s.reference.as_str(), s.value.as_str())).collect();
    // Ours: what the state file lists, and env files that start with this plugin's header (so a lost state file doesn't
    // strand them). A plain `file` has no header: if the state file is lost, it's reported as someone else's.
    let mut ours: Vec<PathBuf> = written_before.to_vec();
    for o in outputs.iter().filter(|o| o.format == Format::Env && !written_before.contains(&o.path)) {
        if std::fs::read_to_string(&o.path).is_ok_and(|t| t.starts_with(ENV_HEADER)) {
            ours.push(o.path.clone());
        }
    }
    let written_before = ours.as_slice();
    let mut written = vec![];
    let mut missing = BTreeSet::new();
    let mut errors = vec![];
    for o in outputs {
        let lacking: Vec<&str> = o.references().into_iter().filter(|r| !got.contains_key(r)).collect();
        let mine = written_before.contains(&o.path);
        if !lacking.is_empty() {
            missing.extend(lacking.iter().map(|r| r.to_string()));
            if mine
                && let Err(e) = std::fs::remove_file(&o.path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(format!("removing {}: {e}", o.path.display()));
                written.push(o.path.clone());
            }
            continue;
        }
        let content = match o.format {
            Format::Env => env_file(&o.values, &got),
            Format::File => Ok(got[o.reference.as_deref().unwrap_or_default()].to_string()),
        };
        match content.and_then(|c| write::owned(&o.path, &c, written_before, "fleetlayer-1password")) {
            Ok(()) => written.push(o.path.clone()),
            Err(e) => {
                if mine {
                    written.push(o.path.clone());
                }
                errors.push(e);
            }
        }
    }
    for (p, e) in write::remove_stale(written_before, &outputs.iter().map(|o| o.path.clone()).collect::<Vec<_>>()) {
        errors.push(format!("removing {}: {e}", p.display()));
        written.push(p);
    }
    (written, missing.into_iter().collect(), errors)
}

fn when(t: Timestamp) -> String {
    t.strftime("%Y-%m-%d %H:%M UTC").to_string()
}

/// A target's problem, for a person, or None.
fn receive_problem(rc: &Receive, st: &Received, now: Timestamp, started: Timestamp) -> Option<String> {
    if let Some(e) = &st.error {
        return Some(format!("1password: {e}"));
    }
    if !st.missing.is_empty() {
        return Some(format!(
            "1password: this node's files need {}, which isn't granted to this node (or isn't in 1Password); \
             their files were removed",
            st.missing.join(", ")
        ));
    }
    let stale = Duration::from_secs(rc.stale_hours * 3600);
    let Some(applied) = st.applied_at else {
        return (now.duration_since(started).unsigned_abs() > stale).then(|| {
            format!(
                "1password: nothing has arrived since this plugin started at {}; is the 1Password source running, \
                 does a role name this node, and is its delivery approved?",
                when(started)
            )
        });
    };
    (now.duration_since(applied).unsigned_abs() > stale).then(|| {
        format!(
            "1password: nothing has arrived since {} (over {} hours); is the 1Password source running?",
            when(applied),
            rc.stale_hours
        )
    })
}

struct Plugin {
    home: PathBuf,
    node: String,
    receive: Option<Receive>,
    started: Timestamp,
    received: Mutex<Received>,
    source_problem: Mutex<Option<String>>,
    /// The last bundle refused for coming from the wrong node, and when.
    refused: Mutex<Option<(String, Timestamp)>>,
}

impl Plugin {
    fn state_dir(&self) -> PathBuf {
        self.home.join(".local/state/fleetlayer/1password")
    }

    fn save_received(&self, st: &Received) {
        let path = self.state_dir().join("received.json");
        if let Err(e) = write::atomic(&path, &serde_json::to_string_pretty(st).unwrap_or_default()) {
            tracing::warn!("saving {}: {e}", path.display());
        }
    }

    fn load_received(&self) -> Received {
        std::fs::read(self.state_dir().join("received.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn load_source(&self) -> SourceDisk {
        std::fs::read(self.state_dir().join("source.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_source(&self, s: &SourceDisk) {
        let path = self.state_dir().join("source.json");
        if let Err(e) = write::atomic(&path, &serde_json::to_string_pretty(s).unwrap_or_default()) {
            tracing::warn!("saving {}: {e}", path.display());
        }
    }

    /// Apply a bundle on this target.
    async fn apply(&self, version: u64, body: Body) -> Result<usize, String> {
        let rc = self.receive.clone().unwrap_or_default();
        let mut st = self.received.lock().await;
        let secrets: Vec<Secret> =
            body.secrets.iter().map(|s| Secret { reference: s.reference.clone(), value: s.value.clone() }).collect();
        let mut errors = vec![];
        if let Err(e) = registry::register(&self.home, SOURCE, &secrets) {
            errors.push(format!("the secrets registry: {e}"));
        }
        let (written, missing, errs) = apply_outputs(&rc.write, &body.secrets, &st.written);
        errors.extend(errs);
        st.written = written;
        st.missing = missing;
        st.version = version;
        st.read_at = Some(body.read_at);
        st.items = body.secrets.iter().map(|s| (s.reference.clone(), s.updated_at.clone())).collect();
        if errors.is_empty() {
            st.applied_at = Some(Timestamp::now());
            st.error = None;
        } else {
            st.error = Some(errors.join("; "));
        }
        let snapshot = st.clone();
        drop(st);
        self.save_received(&snapshot);
        tracing::info!(version, items = snapshot.items.len(), files = snapshot.written.len(),
            missing = ?snapshot.missing, errors = errors.len(), "received");
        if errors.is_empty() { Ok(snapshot.items.len()) } else { Err(errors.join("; ")) }
    }

    /// Remove everything delivered here: the files written, the registry list, what was received.
    async fn forget(&self) -> Result<(), String> {
        let mut st = self.received.lock().await;
        let mut errors: Vec<String> =
            write::remove_stale(&st.written, &[]).into_iter().map(|(p, e)| format!("{}: {e}", p.display())).collect();
        if let Err(e) = registry::register(&self.home, SOURCE, &[]) {
            errors.push(format!("the secrets registry: {e}"));
        }
        *st = Received::default();
        let snapshot = st.clone();
        drop(st);
        self.save_received(&snapshot);
        tracing::warn!(errors = errors.len(), "forgot everything delivered to this node");
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-1password runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config = load(&home, &node.config).map_err(RpcError::refused)?;
                let roles = Roles {
                    sync_target: config
                        .receive
                        .is_some()
                        .then(|| SyncRole { bundles: vec![format!("{BUNDLE_PREFIX}{}", node.node)] }),
                    sync_source: config.source.as_ref().map(|s| SyncRole {
                        bundles: grants_by_node(&s.roles).keys().map(|n| format!("{BUNDLE_PREFIX}{n}")).collect(),
                    }),
                    ..Default::default()
                };
                let plugin = Arc::new(Plugin {
                    home: home.clone(),
                    node: node.node.clone(),
                    receive: config.receive.clone(),
                    started: Timestamp::now(),
                    received: Mutex::new(Received::default()),
                    source_problem: Mutex::new(None),
                    refused: Mutex::new(None),
                });
                *plugin.received.lock().await = plugin.load_received();
                let first = self.0.lock().await.replace(plugin.clone()).is_none();
                if first {
                    if let Some(s) = config.source {
                        tokio::spawn(source_loop(plugin.clone(), s, peer.clone()));
                    }
                    tokio::spawn(status_loop(plugin.clone(), peer.clone()));
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
                        name: "fleetlayer-1password".into(),
                        title: Some("1Password".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SYNC_APPLY => {
                let a: SyncApply = params(p)?;
                let plugin = self.0.lock().await.clone().ok_or_else(|| RpcError::refused("not initialised"))?;
                let Some(rc) = plugin.receive.clone() else {
                    return Err(RpcError::not_here(format!("this plugin doesn't receive {}", a.bundle)));
                };
                if a.bundle != format!("{BUNDLE_PREFIX}{}", plugin.node) {
                    return Err(RpcError::not_here(format!("this plugin doesn't receive {}", a.bundle)));
                }
                let pins = plugin.state_dir().join("sources.json");
                if let Err(e) = fleetlayer_secrets::pin::check(&pins, &a.bundle, a.from.as_deref(), Some(&rc.from)) {
                    tracing::error!("{e}");
                    *plugin.refused.lock().await = Some((e.clone(), Timestamp::now()));
                    return Ok(json!({"applied": false, "error": e}));
                }
                let body: Body = match a.items.iter().find(|i| i.name == ITEM).map(|i| serde_json::from_str(&i.content))
                {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => return Ok(json!({"applied": false, "error": format!("{ITEM}: {e}")})),
                    None => return Ok(json!({"applied": false, "error": format!("the bundle has no {ITEM}")})),
                };
                match plugin.apply(a.version, body).await {
                    Ok(n) => Ok(json!({"applied": true, "items": n})),
                    Err(e) => Ok(json!({"applied": false, "error": e})),
                }
            }
            ext::method::SYNC_FORGET => {
                let plugin = self.0.lock().await.clone().ok_or_else(|| RpcError::refused("not initialised"))?;
                match plugin.forget().await {
                    Ok(()) => Ok(json!({"forgotten": true})),
                    Err(e) => Ok(json!({"forgotten": false, "error": e})),
                }
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Hand a node its bundle. Answers whether the node applied it now (false: it's offline, waiting for approval or
/// refused it, and the source's node keeps trying).
async fn publish(peer: &Peer, node: &str, version: u64, body: &Body) -> Result<bool, String> {
    let publish = SyncPublish {
        bundle: format!("{BUNDLE_PREFIX}{node}"),
        version,
        secret: true,
        targets: vec![ext::SyncTarget { node: node.to_string() }],
        items: vec![SyncItem {
            name: ITEM.into(),
            content: serde_json::to_string(body).unwrap_or_default(),
            mode: Some(0o600),
            link: None,
        }],
    };
    let r: Value = peer
        .request_within(ext::method::SYNC_PUBLISH, &publish, Duration::from_secs(90))
        .await
        .map_err(|e| format!("handing the bundle for {node} to the node: {e}"))?;
    let mut applied = false;
    for f in r["results"].as_array().into_iter().flatten() {
        if f["ok"] == true {
            applied = true;
        } else {
            tracing::warn!(target = %f["target"], error = %f["error"], version,
                "a target didn't get this version; the node tries it again");
        }
    }
    Ok(applied)
}

/// Every `every_minutes`: read what changed in 1Password, send each node whose items changed (or after
/// `resend_hours`) its bundle, send nodes that lost every grant an empty one, and publish the grants and a reading.
async fn source_loop(plugin: Arc<Plugin>, s: Source, peer: Peer) {
    let op = Op { bin: s.op.clone() };
    let by_node = grants_by_node(&s.roles);
    let all_refs: BTreeSet<String> = by_node.values().flatten().cloned().collect();
    let mut st = SourceState::default();
    let mut disk = plugin.load_source();
    let mut last: HashMap<String, (String, u64, Timestamp)> = HashMap::new();
    tokio::time::sleep(Duration::from_secs(3)).await;
    while !peer.is_closed() {
        let now = Timestamp::now();
        let mut problems = vec![];
        // Who can read what, for `fleetlayer secrets grants`: one reading per role, references only.
        let grants: Vec<ext::Reading> = s
            .roles
            .iter()
            .map(|r| ext::Reading {
                kind: "secrets.grants".into(),
                key: format!("{SOURCE}/{}", r.role),
                at: now,
                node: None,
                data: json!({"source": SOURCE, "role": r.role, "nodes": r.nodes,
                    "items": r.read.iter().map(|x| json!({"ref": x, "access": "read"})).collect::<Vec<_>>()}),
            })
            .collect();
        peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: grants });

        let mut tokens = vec![];
        for f in &s.token_files {
            match op::read_token(f) {
                Ok(t) => tokens.push(Some(t)),
                Err(e) => {
                    problems.push(e);
                    tokens.push(None);
                }
            }
        }
        let mut readings = vec![];
        let round = collect(&op, &tokens, &all_refs, &mut st).await;
        for (r, why) in &round.missing {
            problems.push(format!("{r}: {why}"));
        }
        problems.extend(round.problems.iter().cloned());
        for (node, refs) in &by_node {
            // A node granted an item that couldn't be read, with no earlier value, waits: a bundle without it would
            // remove it there.
            let waiting: Vec<&String> = refs.iter().filter(|r| round.stuck.contains(*r)).collect();
            let secrets: Vec<Delivered> = refs.iter().filter_map(|r| round.values.get(r).cloned()).collect();
            let d = digest(&secrets);
            let due = last.get(node).is_none_or(|(old, _, at)| {
                *old != d || now.duration_since(*at).unsigned_abs() >= Duration::from_secs(s.resend_hours.max(1) * 3600)
            });
            if !waiting.is_empty() {
                problems.push(format!(
                    "not sending {node} its items until {} can be read",
                    waiting.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", ")
                ));
            } else if due {
                let version = disk.next_version(now);
                let count = secrets.len();
                let body = Body { node: node.clone(), read_at: now, secrets };
                disk.nodes.insert(node.clone());
                plugin.save_source(&disk);
                match publish(&peer, node, version, &body).await {
                    Ok(applied) => {
                        tracing::info!(node, version, items = count, applied, "sent");
                        last.insert(node.clone(), (d, version, now));
                    }
                    Err(e) => problems.push(e),
                }
            }
            let (version, published) =
                last.get(node).map(|(_, v, at)| (Some(*v), Some(at.to_string()))).unwrap_or((None, None));
            readings.push(ext::Reading {
                kind: "onepassword.source".into(),
                key: node.clone(),
                at: now,
                node: None,
                data: json!({"node": node, "items": refs.iter().map(|r| json!({"ref": r,
                    "updatedAt": round.values.get(r).and_then(|v| v.updated_at.clone()),
                    "delivered": round.values.contains_key(r)})).collect::<Vec<_>>(),
                    "version": version, "publishedAt": published, "readAt": now.to_string()}),
            });
        }
        // Nodes that had items and are in no role now: an empty bundle removes them there. A node stays on the list
        // until it has applied that bundle, across restarts, so a node that was offline still loses its items.
        let gone: Vec<String> = disk.nodes.iter().filter(|n| !by_node.contains_key(*n)).cloned().collect();
        for node in gone {
            let body = Body { node: node.clone(), read_at: now, secrets: vec![] };
            let version = disk.next_version(now);
            plugin.save_source(&disk);
            match publish(&peer, &node, version, &body).await {
                Ok(true) => {
                    tracing::info!(node, "removed every item: no role names this node any more");
                    disk.nodes.remove(&node);
                }
                Ok(false) => problems.push(format!(
                    "{node} is in no role now but hasn't applied the bundle that removes its items yet (offline, or \
                     waiting for approval); trying again each round"
                )),
                Err(e) => problems.push(e),
            }
        }
        plugin.save_source(&disk);
        let problem = (!problems.is_empty()).then(|| format!("1password source: {}", problems.join("; ")));
        if let Some(p) = &problem {
            tracing::warn!("{p}");
        }
        if !readings.is_empty() {
            peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings });
        }
        *plugin.source_problem.lock().await = problem;
        tokio::time::sleep(Duration::from_secs(s.every_minutes.max(1) * 60)).await;
    }
}

/// Every 30 seconds: the plugin's status, and the target's `onepassword.receive` reading.
async fn status_loop(plugin: Arc<Plugin>, peer: Peer) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    while !peer.is_closed() {
        let now = Timestamp::now();
        let mut problems: Vec<String> = plugin.source_problem.lock().await.iter().cloned().collect();
        if let Some(rc) = &plugin.receive {
            let st = plugin.received.lock().await.clone();
            let mut problem = receive_problem(rc, &st, now, plugin.started);
            if let Some((e, at)) = plugin.refused.lock().await.clone()
                && now.duration_since(at).unsigned_abs() < Duration::from_secs(24 * 3600)
            {
                problem =
                    Some(format!("1password: {e} (at {}); another node tried to send this node's secrets", when(at)));
            }
            let r = ext::Reading {
                kind: "onepassword.receive".into(),
                key: plugin.node.clone(),
                at: now,
                node: None,
                data: json!({"version": st.version, "appliedAt": st.applied_at.map(|t| t.to_string()),
                    "readAt": st.read_at.map(|t| t.to_string()),
                    "items": st.items.iter().map(|(r, u)| json!({"ref": r, "updatedAt": u})).collect::<Vec<_>>(),
                    "files": st.written, "missing": st.missing, "problem": problem}),
            };
            peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: vec![r] });
            problems.extend(problem);
        }
        let report = ext::PluginStatusReport {
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
            detail: json!({"problems": problems.len()}),
        };
        peer.notify(ext::method::PLUGIN_STATUS, &report);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
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
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Mutex::new(None)))).await;
}

#[cfg(test)]
mod tests;
