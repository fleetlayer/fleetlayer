//! The engine's surroundings: the settings file (read again when it changes), the state file (saved before every
//! send and after), fire-now requests from the CLI, and status reports to the node.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::engine::{self, Engine, Outcome, State};
use crate::settings;

/// How the runner reaches the node: the real one sends `_fleetlayer.dev/message/send` and `plugin/status`.
#[async_trait]
pub trait Node: Send + Sync {
    async fn send(&self, to: &str, text: &str) -> Result<Value, RpcError>;
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
    pub fn fire_dir(&self) -> PathBuf {
        self.state_dir.join("fire")
    }
}

/// A fire-now request the CLI leaves in `<state>/fire/<id>.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct FireRequest {
    pub id: String,
    pub name: String,
}

pub struct Runner {
    pub paths: Paths,
    pub engine: Mutex<Engine>,
    tz: TimeZone,
    /// The settings file's text when it was last read (None: it didn't exist). Compared by content, not by
    /// modification time, so two edits within one timestamp tick are both seen.
    seen: Mutex<Option<Option<String>>>,
    reported: Mutex<Option<(Option<String>, Timestamp)>>,
}

impl Runner {
    pub fn open(paths: Paths, tz: TimeZone) -> anyhow::Result<Runner> {
        std::fs::create_dir_all(paths.fire_dir())?;
        let state: State = match std::fs::read(paths.state_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", paths.state_file().display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        let mut engine = Engine::new(state, settings::Settings::default());
        engine.state.settings_file = Some(paths.settings.display().to_string());
        Ok(Runner { paths, engine: Mutex::new(engine), tz, seen: Mutex::new(None), reported: Mutex::new(None) })
    }

    /// Read the settings file again if it changed. A file that can't be read leaves the last good entries running,
    /// and the problem in the status. No file means no entries.
    pub async fn reload(&self) {
        let read = match std::fs::read_to_string(&self.paths.settings) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                self.engine.lock().await.settings_problem = Some(format!("{}: {e}", self.paths.settings.display()));
                *self.seen.lock().await = None;
                return;
            }
        };
        let mut seen = self.seen.lock().await;
        if seen.as_ref() == Some(&read) {
            return;
        }
        let text = read.clone().unwrap_or_default();
        *seen = Some(read);
        let mut engine = self.engine.lock().await;
        match settings::parse(&text, &self.tz) {
            Ok(s) => {
                tracing::info!(entries = s.entries.len(), "read {}", self.paths.settings.display());
                engine.settings = s;
                engine.settings_problem = None;
            }
            Err(e) => {
                tracing::warn!("{}: {e}", self.paths.settings.display());
                engine.settings_problem = Some(e);
            }
        }
    }

    /// One round: read the settings if they changed, take fire-now requests, send what's due, save, report.
    pub async fn step(&self, now: Timestamp, node: &dyn Node) {
        self.reload().await;
        let mut sends = {
            let mut engine = self.engine.lock().await;
            engine.tick(now)
        };
        for req in self.take_requests() {
            let engine = self.engine.lock().await;
            match engine.fire_now(&req.name, &req.id, now) {
                Ok(s) => sends.push(s),
                Err(e) => {
                    drop(engine);
                    let mut engine = self.engine.lock().await;
                    engine.state.manual.push(engine::ManualResult {
                        id: req.id,
                        name: req.name,
                        at: now,
                        result: Outcome::Refused,
                        error: Some(e),
                    });
                }
            }
        }
        // Saved before sending, with each fire marked in flight: a stop mid-send never sends it twice.
        self.save(now).await;
        for s in &sends {
            let result = node.send(&s.to, &s.text).await;
            let (outcome, error) = engine::classify(&result);
            match &error {
                Some(e) => tracing::warn!(entry = s.name, to = s.to, "{}: {e}", outcome.words()),
                None => tracing::info!(entry = s.name, to = s.to, "sent"),
            }
            self.engine.lock().await.record(s, outcome, error, now);
        }
        self.engine.lock().await.last_tick = Some(now);
        self.save(now).await;
        self.report(now, node, false).await;
    }

    /// Tell the node how it's doing: on every change, and at least once a minute (`force`).
    pub async fn report(&self, now: Timestamp, node: &dyn Node, force: bool) {
        let (problem, detail) = {
            let engine = self.engine.lock().await;
            (engine.problem(now), engine.detail())
        };
        let mut reported = self.reported.lock().await;
        let due = match &*reported {
            Some((p, at)) => *p != problem || force || now.duration_since(*at).as_secs() >= 60,
            None => true,
        };
        if due {
            node.status(problem.clone(), detail);
            *reported = Some((problem, now));
        }
    }

    async fn save(&self, now: Timestamp) {
        let json = {
            let mut engine = self.engine.lock().await;
            engine.refresh(now);
            serde_json::to_vec_pretty(&engine.state)
        };
        let Ok(json) = json else { return };
        if let Err(e) = write_atomic(&self.paths.state_file(), &json) {
            tracing::error!("saving {}: {e}", self.paths.state_file().display());
        }
    }

    fn take_requests(&self) -> Vec<FireRequest> {
        let Ok(dir) = std::fs::read_dir(self.paths.fire_dir()) else { return vec![] };
        let mut out = vec![];
        for f in dir.flatten() {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let read = std::fs::read(&path);
            let _ = std::fs::remove_file(&path);
            match read.ok().and_then(|b| serde_json::from_slice::<FireRequest>(&b).ok()) {
                Some(r) => out.push(r),
                None => tracing::warn!("ignoring a fire request that can't be read: {}", path.display()),
            }
        }
        out
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Run until the node goes away: a round every five seconds, and a status report every minute from a separate task,
/// so a round that hangs still turns the status bad ("stuck since").
pub async fn run(runner: Arc<Runner>, node: Arc<dyn Node>, closed: impl Fn() -> bool + Send + 'static) {
    let (r2, n2) = (runner.clone(), node.clone());
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            every.tick().await;
            r2.report(Timestamp::now(), &*n2, true).await;
        }
    });
    let mut every = tokio::time::interval(std::time::Duration::from_secs(5));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !closed() {
        every.tick().await;
        runner.step(Timestamp::now(), &*node).await;
    }
}
