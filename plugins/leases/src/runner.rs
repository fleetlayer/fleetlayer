//! The authority around the engine: the settings file (read again when it changes), the state file (saved after
//! every change), the local socket the `serve` shim and the admin commands talk to, approvals, the fleet's sessions
//! for liveness, and what goes out (events, messages to holders, readings, the plugin's status).
//!
//! The socket (`<state_dir>/leases.sock`, mode 0600) takes one JSON request per connection:
//! `{"op": "acquire"|"release"|"list"|"show"|"hold"|"wait", "caller": "<tab>@<node>"|null, "args": {...}}` and answers
//! with progress lines `{"progress": "…"}` and then one `{"result": …}` or `{"error": "…"}`. `caller` null is the
//! authority's own admin (`fleetlayer-leases release` on its machine). A connection that keeps a lease (`hold`, a
//! `wait`) keeps it until it closes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_protocol::ext::{Event, Reading};
use fleetlayer_protocol::link::AccessDecision;
use fleetlayer_protocol::rpc::RpcError;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, watch};

use crate::engine::{Change, EndReason, Engine, FleetView, Phase, Refusal, Request, SessionInfo, State, Tie, short};
use crate::settings::{self, Settings};

/// How the authority reaches its node. The real one is `ToNode` in `main.rs`; tests use a fake.
#[async_trait]
pub trait Node: Send + Sync {
    /// The fleet (`_fleetlayer.dev/fleet/query`).
    async fn fleet(&self) -> Result<Value, RpcError>;
    /// `_fleetlayer.dev/access/check` in the node's own tenant.
    async fn access(&self, who: &str, to: &str, why: &str, peek: bool) -> Result<AccessDecision, RpcError>;
    async fn publish(&self, event: Event) -> Result<(), RpcError>;
    async fn send(&self, to: &str, text: &str) -> Result<(), RpcError>;
    fn readings(&self, readings: Vec<Reading>);
    fn status(&self, problem: Option<String>, detail: Value);
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub settings: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("leases.sock")
    }
    pub fn log(&self) -> PathBuf {
        self.state_dir.join("log.jsonl")
    }
}

/// The log of every start and end is cut to its newest half when it passes this size.
const LOG_MAX: u64 = 8 << 20;
/// How often the authority checks TTLs, the fleet and approvals.
pub const ROUND: Duration = Duration::from_secs(15);
/// How long the fleet may be unreadable before it's a problem (session-tied leases then can't end with their
/// sessions).
const FLEET_PROBLEM_AFTER: SignedDuration = SignedDuration::from_mins(5);
/// The longest justification the control plane takes, and so the most of a payload the person can be shown.
const MAX_WHY: usize = 1000;
/// How long an event may wait in the outbox before it's a problem.
const OUTBOX_PROBLEM_AFTER: SignedDuration = SignedDuration::from_mins(5);

pub struct Authority {
    pub paths: Paths,
    /// `plugin:<name>@<node>`: who asks the access policy about payloads.
    pub me: String,
    pub engine: Mutex<Engine>,
    pub settings: Mutex<Settings>,
    settings_problem: Mutex<Option<String>>,
    seen: Mutex<Option<Option<String>>>,
    /// Bumped after every change: callers waiting look again.
    version: watch::Sender<u64>,
    asking: Mutex<HashSet<u64>>,
    fleet_ok_at: Mutex<Option<Timestamp>>,
    fleet_error: Mutex<Option<(Timestamp, String)>>,
    pub last_round: Mutex<Option<Timestamp>>,
    reported: Mutex<Option<(Option<String>, Timestamp)>>,
    /// One flush of the outbox at a time, so events go out in order.
    flushing: Mutex<()>,
    outbox_error: Mutex<Option<String>>,
    node: Arc<dyn Node>,
}

impl Authority {
    pub fn open(paths: Paths, me: String, node: Arc<dyn Node>) -> anyhow::Result<Arc<Authority>> {
        std::fs::create_dir_all(&paths.state_dir)?;
        let state = read_state(&paths)?;
        let a = Arc::new(Authority {
            paths,
            me,
            engine: Mutex::new(Engine::new(state, Timestamp::now())),
            settings: Mutex::new(Settings::default()),
            settings_problem: Mutex::new(None),
            seen: Mutex::new(None),
            version: watch::channel(0).0,
            asking: Mutex::new(HashSet::new()),
            fleet_ok_at: Mutex::new(None),
            fleet_error: Mutex::new(None),
            last_round: Mutex::new(None),
            reported: Mutex::new(None),
            flushing: Mutex::new(()),
            outbox_error: Mutex::new(None),
            node,
        });
        Ok(a)
    }

    /// Read the settings file again if it changed. A file that can't be read keeps the last good kinds, and the
    /// problem in the status. No file means no kinds.
    pub async fn reload(&self) {
        let read = match std::fs::read_to_string(&self.paths.settings) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                *self.settings_problem.lock().await = Some(format!("{}: {e}", self.paths.settings.display()));
                *self.seen.lock().await = None;
                return;
            }
        };
        let mut seen = self.seen.lock().await;
        if seen.as_ref() == Some(&read) {
            return;
        }
        match settings::parse(read.as_deref().unwrap_or("")) {
            Ok(s) => {
                tracing::info!(kinds = s.kinds.len(), "read the settings");
                *self.settings.lock().await = s;
                *self.settings_problem.lock().await = None;
            }
            Err(e) => {
                tracing::warn!("the settings file has problems; keeping the last good kinds:\n{e}");
                *self.settings_problem.lock().await =
                    Some(format!("{}: {}", self.paths.settings.display(), e.replace('\n', "; ")));
            }
        }
        *seen = Some(read);
    }

    async fn save(&self, engine: &mut Engine) {
        engine.state.updated_at = Some(Timestamp::now());
        match serde_json::to_vec_pretty(&engine.state) {
            Ok(bytes) => {
                // The last good state stays beside it, for a state file a disk fault leaves unreadable.
                let (file, backup) = (self.paths.state_file(), self.paths.state_file().with_extension("json.bak"));
                if file.exists() {
                    let _ = std::fs::copy(&file, &backup);
                }
                if let Err(e) = write_atomic(&file, &bytes) {
                    tracing::error!("saving {}: {e}", self.paths.state_file().display());
                }
            }
            Err(e) => tracing::error!("serialising the state: {e}"),
        }
    }

    /// After a change: save, wake waiting callers, and tell whoever needs telling.
    async fn changed(self: &Arc<Self>, engine: &mut Engine, changes: Vec<Change>) {
        self.save(engine).await;
        self.version.send_modify(|v| *v += 1);
        if changes.is_empty() {
            return;
        }
        let settings = self.settings.lock().await.clone();
        let mut queued = false;
        for c in &changes {
            self.log(c);
            let epoch = engine.state.epoch.clone();
            if let Some(e) = self.event_for(&epoch, c) {
                engine.state.outbox.push_back(e);
                queued = true;
            }
        }
        if queued {
            // Saved before it's sent: the event survives a restart or an unlinked node.
            self.save(engine).await;
            let me = self.clone();
            tokio::spawn(async move { me.flush().await });
        }
        for c in changes {
            let me = self.clone();
            tokio::spawn(async move { me.tell_holder(&c).await });
        }
        let readings = self.readings_now(engine, &settings);
        self.node.readings(readings);
    }

    /// Send the outbox in order, removing each event the control plane takes. Stops at the first that fails (not
    /// linked, refused by the policy) and tries again next round; an event the node calls malformed is dropped, as a
    /// retry can't fix it. The dedupe keys make a repeat harmless.
    pub async fn flush(&self) {
        let _one = self.flushing.lock().await;
        loop {
            let Some(e) = self.engine.lock().await.state.outbox.front().cloned() else { return };
            let result = self.node.publish(e.clone()).await;
            let mut engine = self.engine.lock().await;
            match result {
                Ok(()) => {}
                Err(err) if err.code == fleetlayer_protocol::rpc::code::INVALID_PARAMS => {
                    tracing::error!(event = ?e.dedupe_key, "the node won't take this lease event; dropping it: {}", err.message);
                }
                Err(err) => {
                    let mut last = self.outbox_error.lock().await;
                    if last.as_deref() != Some(err.message.as_str()) {
                        tracing::warn!(event = ?e.dedupe_key, "publishing a lease event (kept, tried again each round): {}", err.message);
                    }
                    *last = Some(err.message);
                    return;
                }
            }
            if engine.state.outbox.front().is_some_and(|f| f.dedupe_key == e.dedupe_key) {
                engine.state.outbox.pop_front();
            }
            *self.outbox_error.lock().await = None;
            self.save(&mut engine).await;
        }
    }

    fn log(&self, c: &Change) {
        let line = match c {
            Change::Started { lease, .. } => json!({"at": Timestamp::now(), "change": "started", "lease": lease}),
            Change::Ended { ended, .. } => json!({"at": Timestamp::now(), "change": "ended", "lease": ended}),
        };
        let path = self.paths.log();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let keep = &text[text.len() / 2..];
            let keep = keep.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
            let _ = write_atomic(&path, keep.as_bytes());
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The event a change publishes on its kind's channel, if the kind has one. A lease that never started has no
    /// ended event: there was nothing to undo.
    fn event_for(&self, epoch: &str, c: &Change) -> Option<Event> {
        // The channel the lease got when it started: a kind removed or changed since still ends with an event.
        Some(match c {
            Change::Started { lease, first_of_key, .. } => Event {
                channel: lease.events.clone()?,
                kind: "lease.started".into(),
                subject: Some(lease.kind.clone()),
                at: Timestamp::now(),
                dedupe_key: Some(format!("lease:{}:{epoch}:{}:started", self.me, lease.id)),
                summary: format!("{} lease #{} started for {}: {}", lease.kind, lease.id, lease.holder, lease.reason),
                body: json!({"lease": lease, "seq": 1, "firstOfKey": first_of_key, "authority": self.me}),
            },
            Change::Ended { ended, was_held, last_of_key } => {
                if !was_held {
                    return None;
                }
                Event {
                    channel: ended.lease.events.clone()?,
                    kind: "lease.ended".into(),
                    subject: Some(ended.lease.kind.clone()),
                    at: Timestamp::now(),
                    dedupe_key: Some(format!("lease:{}:{epoch}:{}:ended", self.me, ended.lease.id)),
                    summary: format!(
                        "{} lease #{} of {} ended: {}",
                        ended.lease.kind,
                        ended.lease.id,
                        ended.lease.holder,
                        ended.why.words()
                    ),
                    body: json!({"lease": ended, "seq": 2, "lastOfKey": last_of_key, "authority": self.me}),
                }
            }
        })
    }

    async fn tell_holder(&self, c: &Change) {
        let (lease, text) = match c {
            // A caller waiting on the call hears it there.
            Change::Started { lease, in_call: false, .. } if lease.attached == 0 => (
                lease,
                format!(
                    "Your {} lease #{} has started (until {} at the latest): {}. Release it with `fleetlayer lease \
                     release {}` when you're done.",
                    lease.kind,
                    lease.id,
                    lease.expires_at.map(short).unwrap_or_default(),
                    lease.reason,
                    lease.id
                ),
            ),
            Change::Ended { ended, .. }
                if !matches!(ended.why, EndReason::Released | EndReason::Cancelled) && ended.lease.attached == 0 =>
            {
                let detail = ended.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
                (
                    &ended.lease,
                    format!(
                        "Your {} lease #{} ({}) has ended: {}{detail}.",
                        ended.lease.kind,
                        ended.lease.id,
                        ended.lease.reason,
                        ended.why.words()
                    ),
                )
            }
            _ => return,
        };
        if lease.tie != Tie::Session {
            return;
        }
        if let Err(e) = self.node.send(&lease.holder, &text).await {
            tracing::info!(lease = lease.id, to = %lease.holder, "telling the holder: {}", e.message);
        }
    }

    /// One reading per kind: how many hold, wait and ask, and since when.
    fn readings_now(&self, engine: &Engine, settings: &Settings) -> Vec<Reading> {
        let now = Timestamp::now();
        settings
            .kinds
            .values()
            .map(|k| {
                let of = |p: Phase| engine.state.leases.values().filter(move |l| l.kind == k.name && l.phase == p);
                let oldest = |p: Phase| of(p).map(|l| l.started_at.unwrap_or(l.requested_at)).min();
                let holders: Vec<Value> = of(Phase::Held)
                    .take(10)
                    .map(|l| json!({"id": l.id, "holder": l.holder, "since": l.started_at, "reason": l.reason}))
                    .collect();
                Reading {
                    kind: "lease.kind".into(),
                    key: k.name.clone(),
                    at: now,
                    node: None,
                    data: json!({
                        "slots": k.slots,
                        "held": of(Phase::Held).count(),
                        "waiting": of(Phase::Waiting).count(),
                        "asking": of(Phase::Asking).count(),
                        "oldestHeldSince": oldest(Phase::Held),
                        "oldestWaitingSince": of(Phase::Waiting).map(|l| l.requested_at).min(),
                        "holders": holders,
                    }),
                }
            })
            .collect()
    }

    /// One round: settings, the fleet, every time-based check, approvals, readings and the status.
    pub async fn round(self: &Arc<Self>) {
        self.reload().await;
        let fleet = match self.node.fleet().await {
            Ok(v) => {
                *self.fleet_ok_at.lock().await = Some(Timestamp::now());
                *self.fleet_error.lock().await = None;
                Some(parse_fleet(&v))
            }
            Err(e) => {
                let mut err = self.fleet_error.lock().await;
                if err.is_none() {
                    tracing::warn!("can't read the fleet, so session holders can't be checked: {}", e.message);
                    *err = Some((Timestamp::now(), e.message));
                }
                None
            }
        };
        let settings = self.settings.lock().await.clone();
        let asking: Vec<u64> = {
            let mut engine = self.engine.lock().await;
            let changes = engine.tick(&settings, fleet.as_ref(), Timestamp::now());
            self.changed(&mut engine, changes).await;
            let readings = self.readings_now(&engine, &settings);
            self.node.readings(readings);
            engine.state.leases.values().filter(|l| l.phase == Phase::Asking).map(|l| l.id).collect()
        };
        for id in asking {
            self.ask(id).await;
        }
        self.flush().await;
        *self.last_round.lock().await = Some(Timestamp::now());
        self.report().await;
    }

    /// Ask the access policy about an asking lease's payload, once at a time per lease: `write` on
    /// `service:leases/<kind>/<key>`, as this plugin, with the holder and reason as the justification. The first ask
    /// files the approval request; later ones only look, until the person decides.
    pub async fn ask(self: &Arc<Self>, id: u64) {
        if !self.asking.lock().await.insert(id) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.ask_once(id).await;
            me.asking.lock().await.remove(&id);
        });
    }

    async fn ask_once(self: &Arc<Self>, id: u64) {
        let Some((target, why, filed)) = ({
            let engine = self.engine.lock().await;
            engine.get(id).filter(|l| l.phase == Phase::Asking).map(|l| {
                let target = format!("service:leases/{}/{}", l.kind, l.payload_key.clone().unwrap_or_default());
                let payload = l.payload.as_ref().map(Value::to_string).unwrap_or_default();
                let why = format!("lease #{} for {}: {} — {} {payload}", l.id, l.holder, l.reason, l.kind);
                // Filed once the policy said it waits for the person: later asks only look.
                let filed = l.approval.as_deref().is_some_and(|a| a.contains("waiting for the person"));
                (target, why, filed)
            })
        }) else {
            return;
        };
        // The person must see the whole payload to approve it: one too long to show isn't put to them.
        if why.chars().count() > MAX_WHY {
            let mut engine = self.engine.lock().await;
            let changes = engine.asked(
                id,
                None,
                Some(format!(
                    "its reason and payload are too long to show the person whole (over {MAX_WHY} characters)"
                )),
                Timestamp::now(),
            );
            self.changed(&mut engine, changes).await;
            return;
        }
        let decision = self.node.access(&self.me, &target, &why, filed).await;
        let settings = self.settings.lock().await.clone();
        let mut engine = self.engine.lock().await;
        let now = Timestamp::now();
        let changes = match decision {
            Ok(d) if d.allowed => {
                if filed {
                    // Record the use: the looks before it left no audit entry.
                    let _ = self.node.access(&self.me, &target, &why, false).await;
                }
                engine.approved(&settings, id, now)
            }
            Ok(d) => match decided(&d) {
                Decided::Pending(request) => {
                    engine.asked(id, Some(format!("waiting for the person to approve request {request}")), None, now)
                }
                // The request it waited on is gone (it expired unanswered): file a new one next round.
                Decided::NotAsked => engine.asked(id, Some("asking the access policy again".into()), None, now),
                Decided::Refused => engine.asked(id, None, Some(d.reason), now),
            },
            Err(e) => {
                // Can't ask now (not linked, access.check not granted): it keeps asking, and says why. The control
                // plane's refusals come back as answers, above.
                tracing::warn!(lease = id, "asking the access policy: {}", e.message);
                engine.asked(id, Some(format!("can't ask the access policy now: {}", e.message)), None, now)
            }
        };
        self.changed(&mut engine, changes).await;
    }

    async fn report(&self) {
        let now = Timestamp::now();
        let settings = self.settings.lock().await.clone();
        let engine = self.engine.lock().await;
        let mut problems = vec![];
        if let Some(p) = self.settings_problem.lock().await.clone() {
            problems.push(p);
        }
        for e in engine.recently_expired(now) {
            problems.push(format!(
                "{} lease #{} of {} ran into its TTL at {} without being released ({})",
                e.lease.kind,
                e.lease.id,
                e.lease.holder,
                short(e.ended_at),
                e.lease.reason
            ));
        }
        for (kind, since) in engine.stuck(&settings, now) {
            let holders = engine.holders(&kind);
            problems.push(format!(
                "the {kind} queue is stuck: its first in line has waited since {} ({})",
                short(since),
                if holders.is_empty() { "nobody holds a slot".to_string() } else { holders.join("; ") }
            ));
        }
        if let Some(first) = engine.state.outbox.front()
            && first.at + OUTBOX_PROBLEM_AFTER <= now
        {
            problems.push(format!(
                "{} lease event(s) haven't reached the control plane since {} ({}), so the services acting on them \
                 don't know",
                engine.state.outbox.len(),
                short(first.at),
                self.outbox_error.lock().await.clone().unwrap_or_else(|| "not sent yet".into())
            ));
        }
        if let Some((since, e)) = self.fleet_error.lock().await.clone()
            && since + FLEET_PROBLEM_AFTER <= now
        {
            problems.push(format!(
                "can't read the fleet since {}, so leases can't end with their sessions: {e}",
                short(since)
            ));
        }
        let problem = (!problems.is_empty()).then(|| problems.join("; "));
        let detail = json!({
            "kinds": settings.kinds.len(),
            "held": engine.state.leases.values().filter(|l| l.phase == Phase::Held).count(),
            "waiting": engine.state.leases.values().filter(|l| l.phase == Phase::Waiting).count(),
            "asking": engine.state.leases.values().filter(|l| l.phase == Phase::Asking).count(),
            "outbox": engine.state.outbox.len(),
            "lastRound": now,
            "fleetReadAt": *self.fleet_ok_at.lock().await,
        });
        drop(engine);
        let mut reported = self.reported.lock().await;
        let due = match &*reported {
            Some((p, at)) => *p != problem || now.duration_since(*at).as_secs() >= 55,
            None => true,
        };
        if due {
            self.node.status(problem.clone(), detail);
            *reported = Some((problem, now));
        }
    }

    /// Report a stuck loop: called from a separate task when no round finished for three minutes.
    pub fn stuck(&self, since: Timestamp) {
        self.node.status(Some(format!("its loop is stuck since {}", short(since))), json!({}));
    }

    // ------------------------------------------------------------ requests from the socket

    pub async fn serve(self: Arc<Self>, listener: tokio::net::UnixListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let me = self.clone();
            tokio::spawn(async move { me.connection(stream).await });
        }
    }

    async fn connection(self: Arc<Self>, stream: UnixStream) {
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        // The connection closing ends what it keeps; anything more it sends is ignored.
        let (closed_tx, closed) = watch::channel(false);
        tokio::spawn(async move {
            let mut sink = [0u8; 256];
            while r.read(&mut sink).await.is_ok_and(|n| n > 0) {}
            let _ = closed_tx.send(true);
        });
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let me = self.clone();
        let work = tokio::spawn(async move { me.handle(&line, progress_tx, closed).await });
        tokio::pin!(work);
        let answer = loop {
            tokio::select! {
                Some(p) = progress_rx.recv() => {
                    let l = json!({"progress": p}).to_string() + "\n";
                    if w.write_all(l.as_bytes()).await.is_err() {
                        // Still let the work notice the close and clean up.
                    }
                }
                res = &mut work => break res.unwrap_or_else(|e| Err(format!("internal error: {e}"))),
            }
        };
        let l = match answer {
            Ok(v) => json!({"result": v}),
            Err(e) => json!({"error": e}),
        };
        let _ = w.write_all((l.to_string() + "\n").as_bytes()).await;
    }

    async fn handle(
        self: Arc<Self>,
        line: &str,
        progress: tokio::sync::mpsc::UnboundedSender<String>,
        closed: watch::Receiver<bool>,
    ) -> Result<Value, String> {
        #[derive(Deserialize)]
        struct Req {
            op: String,
            caller: Option<String>,
            #[serde(default)]
            args: Value,
        }
        let req: Req = serde_json::from_str(line).map_err(|e| format!("unreadable request: {e}"))?;
        let id = || req.args["id"].as_u64().ok_or_else(|| "give the lease's id".to_string());
        match req.op.as_str() {
            "acquire" => {
                let caller = req.caller.clone().ok_or("acquire needs a caller")?;
                self.acquire(&caller, &req.args, progress, closed).await
            }
            "release" => {
                let id = id()?;
                let settings = self.settings.lock().await.clone();
                let mut engine = self.engine.lock().await;
                let changes = engine.release(&settings, id, req.caller.as_deref(), Timestamp::now())?;
                self.changed(&mut engine, changes).await;
                Ok(json!({"released": id}))
            }
            "list" => Ok(self.list(req.args["kind"].as_str(), req.args["all"].as_bool().unwrap_or(false)).await),
            "show" => {
                let id = id()?;
                let engine = self.engine.lock().await;
                match (engine.get(id), engine.ended(id)) {
                    (Some(l), _) => Ok(json!({"lease": l, "position": engine.position(id)})),
                    (None, Some(e)) => Ok(json!({"ended": e})),
                    (None, None) => Err(format!("no lease #{id}")),
                }
            }
            "hold" | "wait" => {
                let id = id()?;
                {
                    let engine = self.engine.lock().await;
                    let l = engine.get(id).ok_or_else(|| ended_words(&engine, id))?;
                    if let Some(c) = &req.caller
                        && c.rsplit_once('@').map(|(_, n)| n) != Some(l.holder_node())
                    {
                        return Err(format!("lease #{id} is {}'s: only its own node may keep it", l.holder));
                    }
                }
                if req.op == "hold" { self.hold(id, closed).await } else { self.wait_for(id, progress, closed).await }
            }
            other => Err(format!("unknown op {other}")),
        }
    }

    async fn acquire(
        self: &Arc<Self>,
        caller: &str,
        args: &Value,
        progress: tokio::sync::mpsc::UnboundedSender<String>,
        closed: watch::Receiver<bool>,
    ) -> Result<Value, String> {
        let dur = |k: &str| -> Result<Option<SignedDuration>, String> {
            match args[k].as_str() {
                Some(s) => settings::parse_duration(s).map(Some).map_err(|e| format!("{k}: {e}")),
                None => Ok(None),
            }
        };
        let pid = args["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).filter(|p| *p > 1);
        let tie = match args["tie"].as_str() {
            Some("process") => Some(Tie::Process { pid: pid.ok_or("a process tie needs its pid")? }),
            Some("session") => Some(Tie::Session),
            None => None,
            Some(other) => return Err(format!("tie: session or process, not {other}")),
        };
        let wait = args["wait"].as_bool().unwrap_or(false);
        let req = Request {
            kind: args["kind"].as_str().unwrap_or("").to_string(),
            payload: args.get("payload").filter(|p| !p.is_null()).cloned(),
            reason: args["reason"].as_str().unwrap_or("").to_string(),
            holder: caller.to_string(),
            tie,
            fallback_pid: pid,
            ttl: dur("ttl")?,
            idle: dur("idle")?,
            wait,
        };
        let settings = self.settings.lock().await.clone();
        let id = {
            let mut engine = self.engine.lock().await;
            let (id, changes) = engine.acquire(&settings, req, Timestamp::now()).map_err(|e| match e {
                Refusal::Busy { .. } => format!("busy: {e}"),
                Refusal::Invalid(s) => s,
            })?;
            self.changed(&mut engine, changes).await;
            id
        };
        let phase = self.engine.lock().await.get(id).map(|l| l.phase);
        if phase == Some(Phase::Asking) {
            self.ask(id).await;
        }
        if wait && phase != Some(Phase::Held) {
            return self.wait_for(id, progress, closed).await;
        }
        let engine = self.engine.lock().await;
        let l = engine.get(id).ok_or_else(|| ended_words(&engine, id))?;
        Ok(json!({"lease": l, "position": engine.position(id)}))
    }

    /// Keep the lease attached while the caller waits for it to start; answers once it's held, or why it ended.
    async fn wait_for(
        self: &Arc<Self>,
        id: u64,
        progress: tokio::sync::mpsc::UnboundedSender<String>,
        mut closed: watch::Receiver<bool>,
    ) -> Result<Value, String> {
        if !self.engine.lock().await.attach(id) {
            let engine = self.engine.lock().await;
            return Err(ended_words(&engine, id));
        }
        let mut version = self.version.subscribe();
        let mut said = String::new();
        let out = loop {
            {
                let engine = self.engine.lock().await;
                match engine.get(id) {
                    None => break Err(ended_words(&engine, id)),
                    Some(l) if l.phase == Phase::Held => break Ok(json!({"lease": l})),
                    Some(l) => {
                        let now = match l.phase {
                            Phase::Asking => format!(
                                "waiting for approval: {}",
                                l.approval.as_deref().unwrap_or("asking the access policy")
                            ),
                            _ => {
                                let pos = engine.position(id).unwrap_or(0);
                                let holders = engine.holders(&l.kind);
                                format!("waiting: number {pos} in line for {} (held: {})", l.kind, holders.join("; "))
                            }
                        };
                        if now != said {
                            let _ = progress.send(now.clone());
                            said = now;
                        }
                    }
                }
            }
            tokio::select! {
                r = version.changed() => if r.is_err() { break Err("the authority is stopping".into()) },
                _ = hung_up(&mut closed) => {
                    let settings = self.settings.lock().await.clone();
                    let mut engine = self.engine.lock().await;
                    let changes = engine.detach(&settings, id, Timestamp::now());
                    self.changed(&mut engine, changes).await;
                    return Err("the caller stopped waiting".into());
                }
            }
        };
        let settings = self.settings.lock().await.clone();
        let mut engine = self.engine.lock().await;
        // Started: the caller's wait no longer keeps it (a keeper or the session does).
        let changes = engine.detach(&settings, id, Timestamp::now());
        self.changed(&mut engine, changes).await;
        out
    }

    /// Keep a lease while this connection is open (the CLI's keeper of a process-tied lease); answers when the lease
    /// ends.
    async fn hold(self: &Arc<Self>, id: u64, mut closed: watch::Receiver<bool>) -> Result<Value, String> {
        if !self.engine.lock().await.attach(id) {
            let engine = self.engine.lock().await;
            return Err(ended_words(&engine, id));
        }
        let mut version = self.version.subscribe();
        loop {
            {
                let engine = self.engine.lock().await;
                if engine.get(id).is_none() {
                    let e = engine.ended(id);
                    return Ok(json!({"ended": e}));
                }
            }
            tokio::select! {
                r = version.changed() => if r.is_err() { return Err("the authority is stopping".into()) },
                _ = hung_up(&mut closed) => {
                    let settings = self.settings.lock().await.clone();
                    let mut engine = self.engine.lock().await;
                    let changes = engine.detach(&settings, id, Timestamp::now());
                    self.changed(&mut engine, changes).await;
                    return Err("closed".into());
                }
            }
        }
    }

    pub async fn list(&self, kind: Option<&str>, all: bool) -> Value {
        let settings = self.settings.lock().await.clone();
        let engine = self.engine.lock().await;
        let kinds: Vec<Value> = settings
            .kinds
            .values()
            .filter(|k| kind.is_none_or(|n| n == k.name))
            .map(|k| {
                let of = |p: Phase| -> Vec<&crate::engine::Lease> {
                    engine.state.leases.values().filter(|l| l.kind == k.name && l.phase == p).collect()
                };
                json!({
                    "name": k.name,
                    "description": k.description,
                    "slots": k.slots,
                    "approval": k.approval,
                    "ttl": crate::engine::words(k.ttl),
                    "idle": crate::engine::words(k.idle),
                    "held": of(Phase::Held),
                    "waiting": of(Phase::Waiting),
                    "asking": of(Phase::Asking),
                })
            })
            .collect();
        let ended: Vec<&crate::engine::Ended> = engine
            .state
            .ended
            .iter()
            .rev()
            .filter(|e| kind.is_none_or(|n| n == e.lease.kind))
            .take(if all { usize::MAX } else { 10 })
            .collect();
        // Leases of kinds no longer in the settings (still held until they end).
        let orphans: Vec<&crate::engine::Lease> =
            engine.state.leases.values().filter(|l| !settings.kinds.contains_key(&l.kind)).collect();
        json!({"kinds": kinds, "ended": ended, "unlisted": orphans, "authority": self.me})
    }
}

/// What a refused access decision means for an asking lease.
#[derive(Debug, PartialEq)]
pub enum Decided {
    /// Waiting for the person, on this request.
    Pending(String),
    /// Only looked, and no request is filed (it expired): file one.
    NotAsked,
    Refused,
}

/// Read a decision that isn't `allowed`: from its fields (control planes from v0.4.0), else from its wording.
pub fn decided(d: &AccessDecision) -> Decided {
    if let Some(r) = &d.pending {
        return Decided::Pending(r.clone());
    }
    if d.refused.is_some() {
        return Decided::Refused;
    }
    if d.not_asked {
        return Decided::NotAsked;
    }
    // An older control plane says it only in words.
    if let Some(rest) = d.reason.split("waiting for the person to approve request ").nth(1) {
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        return Decided::Pending(id);
    }
    if d.reason.contains("not asked yet") {
        return Decided::NotAsked;
    }
    Decided::Refused
}

/// Resolves when the caller's connection closes (the watch's guard isn't held across awaits: it isn't `Send`).
async fn hung_up(closed: &mut watch::Receiver<bool>) {
    let _ = closed.wait_for(|c| *c).await;
}

fn ended_words(engine: &Engine, id: u64) -> String {
    match engine.ended(id) {
        Some(e) => {
            let detail = e.detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
            format!("lease #{id} has ended ({}{detail})", e.why.words())
        }
        None => format!("no lease #{id}"),
    }
}

/// The fleet listing reduced to what liveness needs: online nodes and running sessions by tab.
pub fn parse_fleet(v: &Value) -> FleetView {
    let mut f = FleetView::default();
    for n in v["nodes"].as_array().into_iter().flatten() {
        if n["online"].as_bool() == Some(true)
            && let Some(name) = n["name"].as_str()
        {
            f.online.insert(name.to_string());
        }
    }
    for s in v["sessions"].as_array().into_iter().flatten() {
        let running = s["ended"].is_null() && s["meta"]["running"].as_bool().unwrap_or(true);
        let (Some(node), Some(tab)) = (s["node"].as_str(), s["meta"]["tab"].as_str()) else { continue };
        if !running {
            continue;
        }
        let status = s["meta"]["status"].as_str().unwrap_or("unknown");
        let info = SessionInfo {
            resting: matches!(status, "idle" | "done" | "blocked"),
            since: s["meta"]["statusChangedAt"].as_str().and_then(|t| t.parse().ok()),
        };
        // Two sessions in tabs of one name: the one at work counts.
        let key = (node.to_string(), tab.to_string());
        match f.sessions.get(&key) {
            Some(existing) if !existing.resting => {}
            _ => {
                f.sessions.insert(key, info);
            }
        }
    }
    f
}

/// Replace a file so that a crash or a power cut leaves the old or the new one whole: write a temporary file, flush
/// it to disk, rename it over, and flush the directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// The state file, or its backup (the state before the last save) when the file itself can't be read.
fn read_state(paths: &Paths) -> anyhow::Result<State> {
    let read = |p: &Path| -> anyhow::Result<Option<State>> {
        match std::fs::read(p) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", p.display())),
        }
    };
    let backup = paths.state_file().with_extension("json.bak");
    match read(&paths.state_file()) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Ok(read(&backup).unwrap_or(None).unwrap_or_default()),
        Err(e) => match read(&backup) {
            Ok(Some(s)) => {
                tracing::error!("{e}; starting from the backup {} instead", backup.display());
                Ok(s)
            }
            _ => Err(anyhow::anyhow!("{e} (and no readable backup at {}): fix or remove it", backup.display())),
        },
    }
}
