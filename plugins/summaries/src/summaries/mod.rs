//! The summaries module: writes this machine's session summaries and catch-ups.
//!
//! A port of claude-summarizer (github.com/ndbroadbent/claude-summarizer, MIT, same author), first moved into
//! Quietdeck's node and now Fleetlayer's summaries plugin. The records it hands back are in
//! `fleetlayer_protocol::summary`; `docs/plugins/summaries.md` is the contract.
//!
//! The plugin (`main.rs`):
//! - builds a [`SummariesConfig`] from its config section and a [`SummariesEnv`];
//! - sends a [`SessionSignal::Status`] for every session the node knows when it starts, then one per status change
//!   the node reports, and a [`SessionSignal::Closed`] when a session ends;
//! - takes every [`SummaryEvent`] the module sends: writes summaries as Markdown files and publishes them;
//! - calls [`SummariesHandle::catchup`] when asked for a session's catch-up (the chat view's header).
//!
//! Inside:
//! - `transcript`: discovery, reading from a byte offset, the skips and the noise filter;
//! - `condense`: the "worth a call" threshold, the 48,000-character cap and its trimming order, day and week chunks;
//! - `prompt`: claude-summarizer's prompt and schema, word for word, and the catch-up's;
//! - `accounts`: "soonest reset" from the coordinator's `/state`;
//! - `call`: headless Claude Code with the lean flags;
//! - `engine`: one pass over a transcript, offsets, three strikes per chunk;
//! - `schedule`: the idle trigger, the gap, one pending summary per session, the concurrency limit;
//! - `catchup`, `import`, `state`.

// The import and some helpers are used only by tests until the plugin offers them.
#![allow(dead_code)]

pub mod accounts;
pub mod call;
pub mod catchup;
pub mod condense;
pub mod engine;
pub mod import;
pub mod prompt;
pub mod schedule;
pub mod state;
pub mod transcript;

pub use transcript::set_auto_continue_message;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fleetlayer_protocol::ext::Status as AgentStatus;
use fleetlayer_protocol::summary::{Catchup, SessionFacts, SessionSignal, SummariesConfig, SummaryEvent};
use jiff::tz::TimeZone;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::Instant;

use call::{ClaudeCli, Model};
use engine::{Accounts, Config, Engine, Stop};
use prompt::SessionInfo;
use schedule::{Limits, Scheduler, Status};
use transcript::{Agent, Roots, Source};

/// Machine-specific inputs that aren't user settings.
#[derive(Debug, Clone)]
pub struct SummariesEnv {
    /// The module's own state (offsets, last summary per session, failure counts, the catch-up cache). The node
    /// passes `<node state dir>/summaries`.
    pub state_dir: PathBuf,
    /// The user's home directory: transcripts are under `<home>/.claude/projects` and `<home>/.codex/sessions`.
    pub home: PathBuf,
    /// The `claude` binary for headless calls.
    pub claude_program: PathBuf,
    /// The account rotator's coordinator socket (`~/.claude/account-rotator/coordinator.sock`), for the "soonest
    /// reset" account. Once the node's usage module serves the same API on that socket, nothing here changes. None
    /// when no rotator is installed: calls use Claude Code's own login.
    pub coordinator_socket: Option<PathBuf>,
    /// The local time zone's IANA name, for hour, day and week periods.
    pub time_zone: String,
}

/// A request for a catch-up for one session.
#[derive(Debug, Clone)]
pub struct CatchupRequest {
    pub session_id: String,
    /// The transcript, when the node has resolved it.
    pub transcript: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum SummariesError {
    #[error("no transcript found for session {0}")]
    NoTranscript(String),
    #[error("the summaries module has stopped")]
    Stopped,
    #[error("{0}")]
    Other(String),
}

/// A running summaries module.
#[derive(Clone)]
pub struct SummariesHandle {
    tx: mpsc::UnboundedSender<Command>,
    engine: Arc<Engine>,
}

impl SummariesHandle {
    /// The session's catch-up. Answers at once from the cache, with `stale: true` and a regeneration started when
    /// the transcript has grown since; the regenerated one also arrives as [`SummaryEvent::Catchup`]. Writes one and
    /// waits for it only when nothing is cached.
    pub async fn catchup(&self, req: CatchupRequest) -> Result<Catchup, SummariesError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Catchup { req, reply }).map_err(|_| SummariesError::Stopped)?;
        rx.await.map_err(|_| SummariesError::Stopped)?
    }

    /// The one-off import of claude-summarizer's history (`index.json` and the Markdown updates under `dir`), and its
    /// byte offsets (`~/.local/state/claude-summarizer/state.json`) so nothing it already summarised is summarised
    /// again. Sends every record as [`SummaryEvent::Summary`] with `origin: claude_summarizer`, and returns how many
    /// it sent.
    pub async fn import_claude_summarizer(&self, dir: PathBuf) -> Result<usize, SummariesError> {
        let old_state = self.engine.config.roots.home_path().join(".local/state/claude-summarizer/state.json");
        let imported = {
            let mut state = self.engine.state.lock().expect("state lock");
            import::import(&dir, Some(&old_state), &mut state)
                .map_err(|e| SummariesError::Other(format!("{}: {e}", dir.display())))?
        };
        self.engine.save();
        for p in &imported.problems {
            tracing::warn!("summaries import: {p}");
        }
        tracing::info!(
            "summaries import: {} sessions, {} updates, {} transcript offsets",
            imported.sessions,
            imported.records.len(),
            imported.offsets
        );
        let n = imported.records.len();
        for record in imported.records {
            self.engine.sink.send(SummaryEvent::Summary(record)).await.map_err(|_| SummariesError::Stopped)?;
        }
        // Sweep now for whatever the imported offsets leave unread.
        let _ = self.tx.send(Command::Sweep);
        Ok(n)
    }
}

/// Starts the module on the current tokio runtime. It runs until `signals` closes.
pub fn spawn(
    config: SummariesConfig,
    env: SummariesEnv,
    signals: mpsc::Receiver<SessionSignal>,
    events: mpsc::Sender<SummaryEvent>,
) -> SummariesHandle {
    let tz = TimeZone::get(&env.time_zone).unwrap_or_else(|_| TimeZone::system());
    let user = if config.user_name.trim().is_empty() { "the user".to_owned() } else { config.user_name.clone() };
    let mut c = Config::new(&env.home, &env.state_dir, &user);
    c.tz = tz;
    c.idle_delay = Duration::from_secs(config.idle_seconds);
    c.min_gap = Duration::from_secs(config.min_gap_seconds);
    c.max_concurrent = config.max_concurrent.max(1) as usize;
    c.budget = config.input_cap_chars as usize;
    c.daily_days = i64::from(config.daily_chunk_days);
    c.sweep = config.sweep;
    let model: Arc<dyn Model> = Arc::new(ClaudeCli {
        program: env.claude_program.clone(),
        model: config.model.clone(),
        work_dir: env.state_dir.join("work"),
        timeout: Duration::from_secs(420),
    });
    // No socket configured: Claude Code's own login (a path that doesn't exist means the same to `accounts::pick`).
    let accounts =
        Accounts::Coordinator(env.coordinator_socket.clone().unwrap_or_else(|| env.state_dir.join("no-coordinator")));
    start(c, model, accounts, signals, events, config.enabled)
}

/// `spawn` with the model and accounts given: tests use a fake model.
pub fn start(
    config: Config,
    model: Arc<dyn Model>,
    accounts: Accounts,
    signals: mpsc::Receiver<SessionSignal>,
    events: mpsc::Sender<SummaryEvent>,
    enabled: bool,
) -> SummariesHandle {
    let engine = match Engine::new(config.clone(), model.clone(), accounts.clone(), events.clone()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            tracing::error!("summaries: can't read {}: {e}; starting from empty state", config.state_file.display());
            let _ = std::fs::rename(&config.state_file, config.state_file.with_extension("unreadable"));
            Arc::new(Engine::new(config, model, accounts, events).expect("empty state loads"))
        }
    };
    {
        let home = engine.config.roots.home_path();
        let seeded = import::seed_from_claude_summarizer(&home, &mut engine.state.lock().expect("state lock"));
        if let Some(n) = seeded {
            engine.save();
            tracing::info!(
                "summaries: adopted claude-summarizer's offsets for {n} transcripts; the import adds its history"
            );
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let driver = Driver::new(engine.clone(), tx.clone(), enabled);
    tokio::spawn(driver.run(rx, signals));
    SummariesHandle { tx, engine }
}

enum Command {
    Catchup {
        req: CatchupRequest,
        reply: oneshot::Sender<Result<Catchup, SummariesError>>,
    },
    CatchupDone(String, Result<Catchup, String>),
    Finished {
        key: String,
        called: bool,
    },
    /// Every account is spent: wait for the next idle event or sweep.
    Paused,
    /// Sweep now (after the import).
    Sweep,
}

/// What a summary task works on: an idle session, or a transcript the sweep found.
enum Job {
    Session(SessionFacts),
    Transcript(Source),
}

struct Driver {
    engine: Arc<Engine>,
    tx: mpsc::UnboundedSender<Command>,
    enabled: bool,
    sched: Scheduler,
    facts: HashMap<String, SessionFacts>,
    sweep_queue: VecDeque<Source>,
    /// Catch-ups being written.
    catchups_writing: HashSet<String>,
    /// Requests with nothing cached, waiting for the catch-up being written.
    catchup_waiters: HashMap<String, Vec<oneshot::Sender<Result<Catchup, SummariesError>>>>,
    catchup_slots: Arc<Semaphore>,
    paused: bool,
}

fn status_of(s: AgentStatus) -> Status {
    match s {
        AgentStatus::Idle => Status::Idle,
        AgentStatus::Done => Status::Done,
        AgentStatus::Working => Status::Working,
        AgentStatus::Blocked => Status::Blocked,
        AgentStatus::Unknown => Status::Unknown,
    }
}

impl Driver {
    fn new(engine: Arc<Engine>, tx: mpsc::UnboundedSender<Command>, enabled: bool) -> Self {
        let c = &engine.config;
        let sched =
            Scheduler::new(Limits { idle_delay: c.idle_delay, min_gap: c.min_gap, max_concurrent: c.max_concurrent });
        Driver {
            engine,
            tx,
            enabled,
            sched,
            facts: HashMap::new(),
            sweep_queue: VecDeque::new(),
            catchups_writing: HashSet::new(),
            catchup_waiters: HashMap::new(),
            catchup_slots: Arc::new(Semaphore::new(1)),
            paused: false,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>, mut signals: mpsc::Receiver<SessionSignal>) {
        let c = self.engine.config.clone();
        // The start-up sweep waits for herdr's snapshot to arrive, so it knows which sessions are running.
        let mut next_sweep = (self.enabled && c.sweep).then(|| Instant::now() + c.start_sweep_after);
        loop {
            let far = Instant::now() + Duration::from_secs(3600);
            let due = self.sched.next_due().map(Instant::from_std).unwrap_or(far);
            let wake = next_sweep.map_or(due, |s| s.min(due));
            tokio::select! {
                sig = signals.recv() => match sig {
                    Some(sig) => self.signal(sig),
                    None => return,
                },
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => return,
                },
                _ = tokio::time::sleep_until(wake) => {
                    if next_sweep.is_some_and(|s| Instant::now() >= s) {
                        next_sweep = Some(Instant::now() + c.sweep_every);
                        self.queue_sweep();
                    }
                }
            }
            if self.enabled {
                self.dispatch();
            }
        }
    }

    fn signal(&mut self, sig: SessionSignal) {
        let now = Instant::now().into_std();
        match sig {
            SessionSignal::Status { session, status, .. } => {
                let status = status_of(status);
                if matches!(status, Status::Idle | Status::Done) {
                    self.paused = false;
                }
                self.sched.on_status(&session.session_id, status, now);
                self.facts.insert(session.session_id.clone(), session);
            }
            SessionSignal::Closed { session_id, .. } => {
                self.paused = false;
                self.sched.on_status(&session_id, Status::Gone, now);
            }
        }
    }

    fn command(&mut self, cmd: Command) {
        let now = Instant::now().into_std();
        match cmd {
            Command::Finished { key, called } => self.sched.on_finished(&key, called, now),
            Command::Paused => self.paused = true,
            Command::Sweep => {
                if self.enabled && self.engine.config.sweep {
                    self.queue_sweep();
                }
            }
            Command::CatchupDone(session_id, result) => {
                self.catchups_writing.remove(&session_id);
                for reply in self.catchup_waiters.remove(&session_id).unwrap_or_default() {
                    let _ = reply.send(result.clone().map_err(SummariesError::Other));
                }
            }
            Command::Catchup { req, reply } => {
                let facts = self.facts.get(&req.session_id).cloned();
                let agent = facts.as_ref().map_or(Agent::Claude, |f| f.agent);
                if let Some(t) = req
                    .transcript
                    .clone()
                    .or_else(|| facts.as_ref().and_then(|f| f.transcript.clone().map(PathBuf::from)))
                {
                    self.engine.remember_transcript(&req.session_id, t);
                }
                let (cached, regenerate) = self.engine.cached_catchup(&req.session_id, agent);
                match cached {
                    Some(c) => {
                        let _ = reply.send(Ok(c));
                    }
                    None if !regenerate => {
                        let _ = reply.send(Err(SummariesError::NoTranscript(req.session_id.clone())));
                        return;
                    }
                    None => self.catchup_waiters.entry(req.session_id.clone()).or_default().push(reply),
                }
                if !regenerate || !self.catchups_writing.insert(req.session_id.clone()) {
                    return;
                }
                let info = SessionInfo {
                    session_id: req.session_id.clone(),
                    agent: Some(agent),
                    tab: facts.as_ref().and_then(|f| f.tab.clone()),
                    dir: facts.as_ref().and_then(|f| f.cwd.clone()),
                };
                let (engine, slots, tx) = (self.engine.clone(), self.catchup_slots.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let session_id = req.session_id;
                    let result = {
                        let _slot = slots.acquire().await.expect("semaphore open");
                        engine.write_catchup(&session_id, agent, info).await
                    };
                    let result = match result {
                        Ok(Some(c)) => {
                            let _ = engine.sink.send(SummaryEvent::Catchup(c.clone())).await;
                            Ok(c)
                        }
                        Ok(None) => Err(format!("no catch-up for {session_id}: no transcript")),
                        Err(stop) => {
                            tracing::info!("summaries: catch-up for {session_id} waits: {stop}");
                            Err(stop.to_string())
                        }
                    };
                    let _ = tx.send(Command::CatchupDone(session_id, result));
                });
            }
        }
    }

    /// Transcripts with unread content whose session isn't running.
    fn queue_sweep(&mut self) {
        self.paused = false;
        let cutoff = SystemTime::now() - Duration::from_secs(self.engine.config.max_age_days as u64 * 86400);
        let running: HashSet<String> = self.sched.running_sessions().into_iter().collect();
        let mut sources = transcript::discover(&self.engine.config.roots, cutoff);
        // Newest first, as claude-summarizer's runs went.
        sources.sort_by_key(|s| std::cmp::Reverse(transcript::modified(&s.path)));
        let queued: HashSet<_> = self.sweep_queue.iter().map(|s| s.path.clone()).collect();
        let before = self.sweep_queue.len();
        for s in sources {
            if queued.contains(&s.path) || !self.engine.has_unread(&s) {
                continue;
            }
            if session_key(&s).is_some_and(|k| running.contains(&k)) {
                continue;
            }
            self.sweep_queue.push_back(s);
        }
        self.engine.state.lock().expect("state lock").last_sweep = Some(jiff::Timestamp::now());
        tracing::info!("summaries: sweep queued {} transcripts", self.sweep_queue.len() - before);
    }

    fn dispatch(&mut self) {
        if self.paused {
            return;
        }
        let now = Instant::now().into_std();
        for sid in self.sched.take_ready(now) {
            let facts = self.facts.get(&sid).cloned().unwrap_or(SessionFacts {
                session_id: sid.clone(),
                agent: Agent::Claude,
                tab: None,
                cwd: None,
                transcript: None,
            });
            self.spawn_job(sid, Job::Session(facts));
        }
        while let Some(source) = self.sweep_queue.front().cloned() {
            let key = session_key(&source).unwrap_or_else(|| source.path.to_string_lossy().into_owned());
            if !self.sched.try_start(&key, now) {
                break;
            }
            self.sweep_queue.pop_front();
            self.spawn_job(key, Job::Transcript(source));
        }
    }

    fn spawn_job(&mut self, key: String, job: Job) {
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        let facts = session_key_facts(&job, &self.facts);
        tokio::spawn(async move {
            let result = match job {
                Job::Session(f) => {
                    if let Some(t) = &f.transcript {
                        engine.remember_transcript(&f.session_id, PathBuf::from(t));
                    }
                    engine.summarise_session(&f.session_id, f.agent, f.tab, f.cwd).await
                }
                Job::Transcript(source) => {
                    let info = SessionInfo {
                        session_id: String::new(),
                        agent: Some(source.agent),
                        tab: facts.as_ref().and_then(|f| f.tab.clone()),
                        dir: facts.and_then(|f| f.cwd),
                    };
                    engine.process(&source, info).await
                }
            };
            let called = match &result {
                Ok(pass) => pass.calls > 0,
                Err(stop) => {
                    tracing::info!("summaries: {key} waits: {stop}");
                    false
                }
            };
            if matches!(result, Err(Stop::NoAccount | Stop::Limit(_))) {
                let _ = tx.send(Command::Paused);
            }
            let _ = tx.send(Command::Finished { key, called });
        });
    }
}

fn session_key_facts(job: &Job, facts: &HashMap<String, SessionFacts>) -> Option<SessionFacts> {
    match job {
        Job::Session(f) => Some(f.clone()),
        Job::Transcript(s) => session_key(s).and_then(|k| facts.get(&k).cloned()),
    }
}

/// The session ID a transcript belongs to, without reading it: Claude Code names the file after it, Codex ends the
/// rollout file's name with it.
pub fn session_key(source: &Source) -> Option<String> {
    let stem = source.path.file_stem()?.to_string_lossy().into_owned();
    match source.agent {
        Agent::Claude => Some(stem),
        Agent::Codex => (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_owned()),
    }
}

impl Roots {
    pub fn home_path(&self) -> PathBuf {
        PathBuf::from(&self.home)
    }
}
