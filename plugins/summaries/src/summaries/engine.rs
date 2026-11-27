//! One summary pass over a transcript: read what it gained since its offset, drop the noise, split it by local day
//! (ISO week for history older than 30 days), and for each chunk worth a call ask the model to rewrite the session's
//! page. Offsets move only after a chunk's call succeeds; a chunk that fails three passes in a row is skipped. Ported
//! from claude-summarizer's `runner.process`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use tokio::sync::mpsc;

use super::accounts::{self, Account};
use super::call::{CallError, CallRequest, Model, Summary, parse_summary};
use super::condense::{self, BUDGET_CHARS};
use super::prompt::{self, SessionInfo};
use super::state::{FileState, State};
use super::transcript::{self, Agent, Roots, Source};
use fleetlayer_protocol::summary::{RecordOrigin, SummaryEvent, SummaryPeriod, SummaryRecord};

pub const MAX_FAILURES: u32 = 3;

#[derive(Debug, Clone)]
pub struct Config {
    /// A fixed "now" for the periods summaries are filed under (tests); the real clock when None.
    pub now: Option<Timestamp>,
    pub roots: Roots,
    pub state_file: PathBuf,
    pub user_name: String,
    pub tz: TimeZone,
    /// Summarise a session once it has been idle this long.
    pub idle_delay: Duration,
    /// At most one summary per session in this window.
    pub min_gap: Duration,
    /// Summary calls at once on this machine.
    pub max_concurrent: usize,
    pub sweep_every: Duration,
    /// Day chunks for this many days, ISO weeks before.
    pub daily_days: i64,
    /// Transcripts last modified longer ago than this are never read.
    pub max_age_days: i64,
    pub budget: usize,
    /// Run a sweep after start-up and every `sweep_every`.
    pub sweep: bool,
    /// The start-up sweep waits this long for herdr's snapshot, so it knows which sessions are running.
    pub start_sweep_after: Duration,
}

impl Config {
    pub fn new(home: &Path, state_dir: &Path, user_name: &str) -> Config {
        Config {
            now: None,
            roots: Roots::from_home(home),
            state_file: state_dir.join("summaries.json"),
            user_name: user_name.to_owned(),
            tz: TimeZone::system(),
            idle_delay: Duration::from_secs(60),
            min_gap: Duration::from_secs(30 * 60),
            max_concurrent: 2,
            sweep_every: Duration::from_secs(24 * 3600),
            daily_days: 30,
            max_age_days: 183,
            budget: BUDGET_CHARS,
            sweep: true,
            start_sweep_after: Duration::from_secs(30),
        }
    }
}

/// Where accounts come from.
#[derive(Debug, Clone)]
pub enum Accounts {
    /// The rotator's coordinator socket (or the node's, serving the same API). Claude Code's own login without one.
    Coordinator(PathBuf),
    /// A fixed answer, for tests: None means every account is spent.
    Fixed(Option<Account>),
    /// Several accounts, for tests: the first that isn't limited.
    List(Vec<Account>),
}

impl Accounts {
    async fn pick(&self, limited: &std::collections::BTreeMap<String, Timestamp>) -> Result<Option<Account>, String> {
        match self {
            Accounts::Coordinator(socket) => {
                accounts::pick(socket, Timestamp::now(), limited).await.map_err(|e| e.to_string())
            }
            Accounts::Fixed(a) => Ok(a.clone().filter(|a| !limited.get(&a.id).is_some_and(|t| *t > Timestamp::now()))),
            Accounts::List(list) => {
                Ok(list.iter().find(|a| !limited.get(&a.id).is_some_and(|t| *t > Timestamp::now())).cloned())
            }
        }
    }
}

/// Why a pass stopped before it finished: the caller tries again on the next idle or sweep.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Stop {
    #[error("no account has room left this week")]
    NoAccount,
    #[error("account choice failed: {0}")]
    Accounts(String),
    #[error("{0}")]
    Limit(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pass {
    pub idle_chunks: u32,
    pub calls: u32,
    pub updates: u32,
    pub no_change: u32,
    pub failures: u32,
}

impl Pass {
    fn add(&mut self, o: &Pass) {
        self.idle_chunks += o.idle_chunks;
        self.calls += o.calls;
        self.updates += o.updates;
        self.no_change += o.no_change;
        self.failures += o.failures;
    }
}

pub struct Engine {
    pub config: Config,
    pub model: Arc<dyn Model>,
    pub accounts: Accounts,
    pub state: Mutex<State>,
    pub sink: mpsc::Sender<SummaryEvent>,
    /// Accounts that answered with a usage limit, left alone until the time given (kept in memory: a restart asks the
    /// readings again, which say the same when they're current).
    pub limited: Mutex<std::collections::BTreeMap<String, Timestamp>>,
}

/// The local hour, floored: `2026-11-23T14`.
pub fn hour_name(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned_datetime(tz.clone()).strftime("%Y-%m-%dT%H").to_string()
}

/// The timeline point a chunk becomes: its ISO week when it's older than the day chunks, its day when it's a day before
/// today, and otherwise the hour of its last real activity (a live summary).
pub fn period_for(
    first: Timestamp,
    real_ts: Timestamp,
    now: Timestamp,
    daily_days: i64,
    tz: &TimeZone,
) -> SummaryPeriod {
    let cutoff = now - jiff::SignedDuration::from_hours(24 * daily_days);
    let date = first.to_zoned_datetime(tz.clone()).date();
    if first < cutoff {
        let w = date.iso_week_date();
        SummaryPeriod::Week(format!("{:04}-W{:02}", w.year(), w.week()))
    } else if date < now.to_zoned_datetime(tz.clone()).date() {
        SummaryPeriod::Day(date.strftime("%Y-%m-%d").to_string())
    } else {
        SummaryPeriod::Hour(hour_name(real_ts, tz))
    }
}

pub fn period_key(p: &SummaryPeriod) -> &str {
    match p {
        SummaryPeriod::Hour(k) | SummaryPeriod::Day(k) | SummaryPeriod::Week(k) => k,
    }
}

impl Engine {
    pub fn new(
        config: Config,
        model: Arc<dyn Model>,
        accounts: Accounts,
        sink: mpsc::Sender<SummaryEvent>,
    ) -> std::io::Result<Engine> {
        let state = State::load(&config.state_file)?;
        Ok(Engine { config, model, accounts, state: Mutex::new(state), sink, limited: Mutex::default() })
    }

    pub(crate) async fn accounts_pick(&self) -> Result<Option<Account>, Stop> {
        let limited = self.limited.lock().expect("limited lock").clone();
        self.accounts.pick(&limited).await.map_err(Stop::Accounts)
    }

    /// The soonest time an account left alone after a usage limit has room again, if any is.
    pub(crate) fn soonest_free(&self) -> Option<Timestamp> {
        let now = Timestamp::now();
        self.limited.lock().expect("limited lock").values().filter(|t| **t > now).min().copied()
    }

    /// `account` answered with a usage limit: the next calls use another until it has room again.
    pub(crate) fn mark_limited(&self, account: &Account) {
        let now = Timestamp::now();
        let until = account.limited_until(now);
        let mut limited = self.limited.lock().expect("limited lock");
        limited.retain(|_, t| *t > now);
        limited.insert(account.id.clone(), until);
        tracing::info!("summaries: leaving {} until {until} (usage limit)", account.label());
    }

    pub fn save(&self) {
        // Several tasks save; one at a time, or two writes share the temporary file and the second rename finds it
        // gone ("can't save …: No such file or directory"), and an older snapshot could land last.
        static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _saving = SAVING.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.state.lock().expect("state lock").clone();
        if let Err(e) = state.save(&self.config.state_file) {
            tracing::error!("summaries: can't save {}: {e}", self.config.state_file.display());
        }
    }

    /// The session's transcripts: the ones already known, plus Claude Code's `<projects>/*/<id>.jsonl` or Codex's
    /// rollout file named with the ID.
    pub fn locate(&self, session_id: &str, agent: Agent) -> Vec<Source> {
        let known = self
            .state
            .lock()
            .expect("state lock")
            .sessions
            .get(session_id)
            .map(|s| s.transcripts.clone())
            .unwrap_or_default();
        let mut found: Vec<PathBuf> = known.into_iter().filter(|p| p.exists()).collect();
        match agent {
            Agent::Claude => {
                if let Ok(dirs) = std::fs::read_dir(&self.config.roots.claude_projects) {
                    for dir in dirs.flatten() {
                        let p = dir.path().join(format!("{session_id}.jsonl"));
                        if p.is_file() && !found.contains(&p) {
                            found.push(p);
                        }
                    }
                }
            }
            Agent::Codex => {
                let cutoff =
                    std::time::SystemTime::now() - Duration::from_secs(self.config.max_age_days as u64 * 86400);
                for s in transcript::discover(&self.config.roots, cutoff) {
                    if s.agent == Agent::Codex
                        && s.path.file_stem().is_some_and(|n| n.to_string_lossy().ends_with(session_id))
                        && !found.contains(&s.path)
                    {
                        found.push(s.path);
                    }
                }
            }
        }
        found.into_iter().map(|path| Source { path, agent }).collect()
    }

    /// A transcript the node resolved for a session.
    pub fn remember_transcript(&self, session_id: &str, path: PathBuf) {
        let mut state = self.state.lock().expect("state lock");
        let s = state.sessions.entry(session_id.to_owned()).or_default();
        if !s.transcripts.contains(&path) {
            s.transcripts.push(path);
        }
    }

    /// Does the transcript hold content no pass has read yet?
    pub fn has_unread(&self, source: &Source) -> bool {
        let Ok(meta) = std::fs::metadata(&source.path) else {
            return false;
        };
        let state = self.state.lock().expect("state lock");
        match state.files.get(&source.path) {
            Some(st) if st.skip.is_some() => false,
            Some(st) => meta.len() != st.offset,
            None => meta.len() > 0,
        }
    }

    /// Summarise every transcript of one session.
    pub async fn summarise_session(
        &self,
        session_id: &str,
        agent: Agent,
        tab: Option<String>,
        dir: Option<String>,
    ) -> Result<Pass, Stop> {
        let mut total = Pass::default();
        for source in self.locate(session_id, agent) {
            let info = SessionInfo {
                session_id: session_id.to_owned(),
                agent: Some(agent),
                tab: tab.clone(),
                dir: dir.clone(),
            };
            total.add(&self.process(&source, info).await?);
        }
        Ok(total)
    }

    fn set_file(&self, path: &Path, st: FileState) {
        self.state.lock().expect("state lock").files.insert(path.to_path_buf(), st);
    }

    /// One pass over one transcript. `info.session_id` may be empty: the transcript's own ID is used.
    pub async fn process(&self, source: &Source, mut info: SessionInfo) -> Result<Pass, Stop> {
        let mut pass = Pass::default();
        let st = self.state.lock().expect("state lock").files.get(&source.path).cloned().unwrap_or_default();
        let Ok(size) = std::fs::metadata(&source.path).map(|m| m.len()) else {
            return Ok(pass);
        };
        let offset = if size < st.offset { 0 } else { st.offset }; // rewritten
        if size == offset || st.skip.is_some() {
            return Ok(pass);
        }
        let (src, home) = (source.clone(), self.config.roots.home.clone());
        let res = match tokio::task::spawn_blocking(move || transcript::read(&src, offset, &home)).await {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                tracing::warn!("summaries: {}: {e}", source.path.display());
                return Ok(pass);
            }
            Err(e) => panic!("transcript read panicked: {e}"),
        };
        let Some(session_id) = res.session_id.clone().filter(|_| res.skip.is_none()) else {
            self.set_file(
                &source.path,
                FileState {
                    offset: res.end,
                    skip: Some(res.skip.unwrap_or_else(|| "no session ID".into())),
                    ..Default::default()
                },
            );
            self.save();
            return Ok(pass);
        };
        info.session_id = session_id.clone();
        info.agent = Some(source.agent);
        if info.dir.is_none() {
            info.dir = res.cwd.clone();
        }
        {
            let mut state = self.state.lock().expect("state lock");
            let s = state.sessions.entry(session_id.clone()).or_default();
            if !s.transcripts.contains(&source.path) {
                s.transcripts.push(source.path.clone());
            }
        }

        let tz = &self.config.tz;
        let now = self.config.now.unwrap_or_else(Timestamp::now);
        for chunk in condense::chunk_by_period(&res.events, self.config.daily_days, now, tz) {
            let chunk_end = chunk.last().expect("chunks aren't empty").end;
            if !condense::is_real(&chunk) {
                pass.idle_chunks += 1;
                self.set_file(&source.path, FileState { offset: chunk_end, ..Default::default() });
                continue;
            }
            let account = match self.accounts_pick().await {
                Ok(Some(a)) => a,
                Ok(None) => {
                    self.save();
                    return Err(Stop::NoAccount);
                }
                Err(e) => {
                    self.save();
                    return Err(e);
                }
            };
            let (page, page_hour) = {
                let state = self.state.lock().expect("state lock");
                let s = &state.sessions[&session_id];
                (s.page.clone(), s.page_hour.clone())
            };
            let span = condense::span(&chunk, tz);
            let user_prompt = prompt::build_prompt(
                &info,
                (!page.is_empty()).then_some((page.as_str(), page_hour.as_str())),
                &condense::condense(&chunk, self.config.budget, tz),
                &span,
            );
            let real_ts = condense::last_real_ts(&chunk).unwrap_or(chunk.last().expect("non-empty").ts);
            let period = period_for(chunk[0].ts, real_ts, now, self.config.daily_days, tz);
            pass.calls += 1;
            let req = CallRequest {
                system_prompt: prompt::summary_system(&self.config.user_name),
                schema: prompt::SUMMARY_SCHEMA.to_owned(),
                prompt: user_prompt.clone(),
                account: account.clone(),
            };
            let label = info.tab.clone().unwrap_or_else(|| session_id.clone());
            let result = self.model.call(req).await.and_then(|(v, stats)| parse_summary(&v).map(|s| (s, stats)));
            let (summary, stats) = match result {
                Ok(ok) => ok,
                Err(error) => {
                    pass.failures += 1;
                    let fails = if st.failed_at == Some(chunk_end) { st.failures + 1 } else { 1 };
                    tracing::warn!(
                        "summaries: {label}: call failed ({fails}/{MAX_FAILURES}) on {}: {error}",
                        account.label()
                    );
                    if let CallError::Limit(msg) = error {
                        self.mark_limited(&account);
                        self.save();
                        return Err(Stop::Limit(msg));
                    }
                    if fails >= MAX_FAILURES {
                        tracing::warn!("summaries: {label}: skipping {span} after {fails} failures");
                        self.set_file(&source.path, FileState { offset: chunk_end, ..Default::default() });
                        continue;
                    }
                    let current = self
                        .state
                        .lock()
                        .expect("state lock")
                        .files
                        .get(&source.path)
                        .map(|f| f.offset)
                        .unwrap_or(offset);
                    self.set_file(
                        &source.path,
                        FileState { offset: current, failed_at: Some(chunk_end), failures: fails, skip: None },
                    );
                    self.save();
                    return Ok(pass);
                }
            };
            let last_activity = {
                let mut state = self.state.lock().expect("state lock");
                let s = state.sessions.get_mut(&session_id).expect("inserted above");
                let last = s.last_activity.map_or(real_ts, |t| t.max(real_ts));
                s.last_activity = Some(last);
                if let Summary::Update { page, .. } = &summary {
                    s.page = page.clone();
                    s.page_hour = period_key(&period).to_owned();
                }
                last
            };
            let status = if matches!(summary, Summary::Update { .. }) { "update" } else { "no_change" };
            match summary {
                Summary::Update { page, paragraph, sentence } => {
                    pass.updates += 1;
                    let _ = self
                        .sink
                        .send(SummaryEvent::Summary(SummaryRecord {
                            session_id: session_id.clone(),
                            agent: source.agent,
                            period: period.clone(),
                            activity_from: chunk[0].ts,
                            activity_to: real_ts,
                            written_at: Timestamp::now(),
                            sentence,
                            paragraph,
                            page,
                            tab: info.tab.clone(),
                            dir: info.dir.clone(),
                            origin: RecordOrigin::Node,
                        }))
                        .await;
                }
                Summary::NoChange => pass.no_change += 1,
            }
            let _ = self.sink.send(SummaryEvent::Activity { session_id: session_id.clone(), last_activity }).await;
            tracing::info!(
                "summaries: {label}: {} ({span}, {} chars, {} tokens, {}s, {})",
                status,
                user_prompt.chars().count(),
                stats.input_tokens,
                stats.seconds,
                account.label()
            );
            self.set_file(&source.path, FileState { offset: chunk_end, ..Default::default() });
            self.save();
        }
        self.set_file(&source.path, FileState { offset: res.end, ..Default::default() });
        self.save();
        Ok(pass)
    }
}
