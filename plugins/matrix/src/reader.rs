//! The Matrix side: sign in as the bot, sync the configured rooms, turn each new message into an event in the outbox,
//! post what agents send, and keep the health the plugin reports.
//!
//! - The password is read once, to sign in. The device, its access token and its crypto store (rooms may be end-to-end
//!   encrypted) stay in the state directory, mode 700, and never leave this machine.
//! - The first sign-in starts from now: what the rooms held before isn't published.
//! - The sync position is the plugin's own (`position.json`), moved on only after the sync's events are saved in the
//!   outbox, so a crash or restart leaves no gap. When the homeserver sends only the latest part of a busy room's
//!   timeline (`limited`), the rest is fetched back to the newest message already seen.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleetlayer_protocol::ext::Event;
use jiff::Timestamp;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::{SyncSettings, SyncToken};
use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::relation::{InReplyTo, Reply};
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomAliasId, OwnedRoomId, UInt, UserId};
use matrix_sdk::{Client, Room, RoomState};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};

use crate::config::{self, Config, expand, read_secret_file};
use crate::outbox::{Outbox, Position, private_dir, write_private};
use crate::render::{self, Message, UNDECRYPTABLE};

const RETRY: Duration = Duration::from_secs(30);
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
/// Events per room one sync carries before it's `limited`.
const TIMELINE_LIMIT: u32 = 100;
/// The most events fetched back to fill one gap.
const BACKFILL_MAX: usize = 1_000;
/// A sync that hasn't worked for this long is a problem.
const SYNC_STALE: Duration = Duration::from_secs(180);
/// An event waiting this long to be published is a problem.
const WAIT_STALE: Duration = Duration::from_secs(600);
/// At most one heartbeat ping per room per this long.
const PING_EVERY: Duration = Duration::from_secs(60);
/// How often to try joining a room the bot isn't in.
const JOIN_EVERY: Duration = Duration::from_secs(600);
/// The most a post's text may be.
pub const MAX_POST: usize = 16 * 1024;

/// What the reader, the publisher, the post socket and the status report share.
pub struct Shared {
    pub cfg: Config,
    pub rooms: Vec<config::Room>,
    pub node: String,
    pub dir: PathBuf,
    /// How an agent replies (the shared tool's `<tool>@<node>`), put in each event.
    pub reply_with: Option<String>,
    pub outbox: Mutex<Outbox>,
    pub ready: Notify,
    pub client: Mutex<Option<Client>>,
    /// Each configured room's ID, once its alias resolves.
    pub ids: Mutex<HashMap<String, OwnedRoomId>>,
    pub health: Mutex<Health>,
}

#[derive(Default)]
pub struct Health {
    pub signed_in: bool,
    pub last_sync: Option<Timestamp>,
    pub last_sync_at: Option<Instant>,
    pub sync_error: Option<String>,
    pub publish_error: Option<String>,
    pub published: u64,
    pub read: u64,
    /// Each configured room's membership, by spec: `joined`, `invited`, `left`, `banned`, `unknown`.
    pub rooms: BTreeMap<String, String>,
    pub heartbeat_error: Option<String>,
    pub started: Option<Instant>,
    /// The last time a join was tried, by room ID.
    pub join_tried: HashMap<String, Instant>,
}

impl Shared {
    pub fn new(cfg: Config, rooms: Vec<config::Room>, node: String, dir: PathBuf, reply_with: Option<String>) -> Self {
        let outbox = Outbox::load(&dir);
        Shared {
            cfg,
            rooms,
            node,
            dir,
            reply_with,
            outbox: Mutex::new(outbox),
            ready: Notify::new(),
            client: Mutex::new(None),
            ids: Mutex::new(HashMap::new()),
            health: Mutex::new(Health { started: Some(Instant::now()), ..Default::default() }),
        }
    }

    /// One line for a person when the plugin isn't doing its job, and the numbers behind it.
    pub async fn status(&self) -> (Option<String>, Value) {
        let h = self.health.lock().await;
        let (waiting, oldest) = {
            let o = self.outbox.lock().await;
            (o.len(), o.first().map(|w| w.since))
        };
        let now = Instant::now();
        let up_for = h.started.map_or(Duration::ZERO, |s| now.duration_since(s));
        let sync_stale = match h.last_sync_at {
            Some(at) => now.duration_since(at) > SYNC_STALE,
            None => up_for > SYNC_STALE,
        };
        let mut problem = None;
        if sync_stale {
            let since = h.last_sync.map_or_else(|| "it started".to_string(), short);
            let why = h.sync_error.as_deref().unwrap_or("no answer from the homeserver");
            problem = Some(format!("Matrix hasn't synced since {since}: {why}"));
        }
        if problem.is_none() {
            let out: Vec<String> =
                h.rooms.iter().filter(|(_, s)| s.as_str() != "joined").map(|(r, s)| format!("{r} ({s})")).collect();
            if !out.is_empty() && h.signed_in && h.last_sync.is_some() {
                problem = Some(format!(
                    "{} isn't in {}: invite it (or unban it) so their messages reach Fleetlayer",
                    self.cfg.user,
                    out.join(", ")
                ));
            }
        }
        if problem.is_none()
            && let Some(since) = oldest
            && Timestamp::now().duration_since(since).as_secs() > WAIT_STALE.as_secs() as i64
        {
            let why = h.publish_error.as_deref().unwrap_or("not taken yet");
            problem =
                Some(format!("{waiting} Matrix message(s) have waited since {} to be published: {why}", short(since)));
        }
        if problem.is_none()
            && let Some(e) = &h.heartbeat_error
        {
            problem = Some(e.clone());
        }
        let detail = json!({
            "signedIn": h.signed_in,
            "lastSyncAt": h.last_sync,
            "syncError": h.sync_error,
            "rooms": h.rooms,
            "read": h.read,
            "published": h.published,
            "waiting": waiting,
            "oldestWaitingSince": oldest,
            "publishError": h.publish_error,
        });
        (problem, detail)
    }

    /// The configured room an agent names: its channel, its spec (ID or alias) or its ID.
    pub async fn find_room(&self, name: &str) -> Result<(config::Room, OwnedRoomId), String> {
        let ids = self.ids.lock().await;
        let room = self
            .rooms
            .iter()
            .find(|r| r.channel == name || r.spec == name || ids.get(&r.spec).is_some_and(|id| id.as_str() == name))
            .ok_or_else(|| {
                let names: Vec<&str> = self.rooms.iter().map(|r| r.channel.as_str()).collect();
                format!("there's no room {name:?} here; the rooms are {}", names.join(", "))
            })?;
        let id = ids
            .get(&room.spec)
            .cloned()
            .ok_or_else(|| format!("{} isn't resolved yet: the bot hasn't signed in to Matrix", room.spec))?;
        Ok((room.clone(), id))
    }

    async fn joined_room(&self, id: &OwnedRoomId) -> Result<Room, String> {
        let client = self.client.lock().await.clone().ok_or("the bot isn't signed in to Matrix yet")?;
        let room = client.get_room(id).ok_or_else(|| format!("{} isn't in {id}", self.cfg.user))?;
        if room.state() != RoomState::Joined {
            return Err(format!("{} isn't in {id} ({})", self.cfg.user, state_word(room.state())));
        }
        Ok(room)
    }

    /// Post `text` to the room an agent names, as a notice unless `kind` is `text`. Answers the new event's ID.
    pub async fn post(&self, name: &str, text: &str, reply_to: Option<&str>, kind: &str) -> Result<String, String> {
        let (room, id) = self.find_room(name).await?;
        if !room.post {
            return Err(format!("posting to {} is turned off (post: false in node.yaml)", room.channel));
        }
        if text.trim().is_empty() {
            return Err("say something: the text is empty".into());
        }
        if text.len() > MAX_POST {
            return Err(format!("the text is over {} KB", MAX_POST / 1024));
        }
        let mut content = match kind {
            "notice" => RoomMessageEventContent::notice_plain(text),
            "text" => RoomMessageEventContent::text_plain(text),
            other => return Err(format!("kind is notice or text, not {other:?}")),
        };
        if let Some(r) = reply_to {
            let ev = OwnedEventId::try_from(r).map_err(|e| format!("reply_to {r:?} isn't an event ID: {e}"))?;
            content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(ev))));
        }
        let joined = self.joined_room(&id).await?;
        let sent = joined.send(content).await.map_err(|e| format!("Matrix refused the message: {e}"))?;
        tracing::info!(room = %room.channel, event = %sent.response.event_id, "posted to Matrix");
        Ok(sent.response.event_id.to_string())
    }

    /// The room's latest messages, oldest first.
    pub async fn history(&self, name: &str, limit: u32) -> Result<Vec<Message>, String> {
        let (_, id) = self.find_room(name).await?;
        let room = self.joined_room(&id).await?;
        let mut opts = MessagesOptions::backward();
        opts.limit = UInt::from(limit.clamp(1, 100));
        let page = room.messages(opts).await.map_err(|e| format!("reading the room: {e}"))?;
        let mut out = vec![];
        for ev in page.chunk.iter().rev() {
            // History includes the bot's own posts: an agent wants to see what was answered.
            if let Some(m) = message(&room, ev, None).await {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Each configured room: channel, spec, ID, name, whether the bot is in it, whether agents may post.
    pub async fn room_list(&self) -> Vec<Value> {
        let ids = self.ids.lock().await.clone();
        let client = self.client.lock().await.clone();
        let mut out = vec![];
        for r in &self.rooms {
            let id = ids.get(&r.spec);
            let room = id.and_then(|id| client.as_ref().and_then(|c| c.get_room(id)));
            out.push(json!({
                "channel": r.channel,
                "room": r.spec,
                "roomId": id.map(|i| i.to_string()),
                "name": room.as_ref().map(room_name),
                "member": room.as_ref().map_or("unknown", |r| state_word(r.state())),
                "post": r.post,
                "routes": r.routes.iter().map(|x| json!({"match": x.pattern.as_str(), "sender": x.sender, "subject": x.subject})).collect::<Vec<_>>(),
            }));
        }
        out
    }
}

fn short(t: Timestamp) -> String {
    t.round(jiff::Unit::Second).unwrap_or(t).to_string()
}

fn state_word(s: RoomState) -> &'static str {
    match s {
        RoomState::Joined => "joined",
        RoomState::Invited => "invited",
        RoomState::Left => "left",
        RoomState::Banned => "banned",
        RoomState::Knocked => "knocked",
    }
}

pub fn room_name(room: &Room) -> String {
    room.cached_display_name()
        .map(|n| n.to_string())
        .or_else(|| room.name())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| room.room_id().to_string())
}

/// A timeline event as a message, or None when it isn't one to pass on: not a room message, an edit, or (with `me`)
/// the bot's own.
async fn message(room: &Room, ev: &TimelineEvent, me: Option<&UserId>) -> Option<Message> {
    let base = |sender: String, event_id: String, ts_ms: i64| Message {
        room_id: room.room_id().to_string(),
        room_name: room_name(room),
        event_id,
        sender,
        sender_name: None,
        text: String::new(),
        ts_ms,
        reply_to: None,
        undecryptable: false,
    };
    let mut m = match &ev.kind {
        TimelineEventKind::UnableToDecrypt { event, .. } => {
            let sender: String = event.get_field("sender").ok().flatten()?;
            let id: String = event.get_field("event_id").ok().flatten()?;
            let ts: i64 = event.get_field("origin_server_ts").ok().flatten().unwrap_or(0);
            tracing::warn!(room = %room.room_id(), event = %id, %sender, "a Matrix message this device can't decrypt");
            Message { text: UNDECRYPTABLE.into(), undecryptable: true, ..base(sender, id, ts) }
        }
        _ => {
            let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncMessageLikeEvent::Original(o),
            ))) = ev.raw().deserialize()
            else {
                return None;
            };
            let reply_to = match &o.content.relates_to {
                Some(Relation::Replacement(_)) => return None,
                Some(Relation::Reply(r)) => Some(r.in_reply_to.event_id.to_string()),
                _ => None,
            };
            let ts = i64::try_from(u64::from(o.origin_server_ts.0)).unwrap_or(0);
            Message {
                text: o.content.body().to_string(),
                reply_to,
                ..base(o.sender.to_string(), o.event_id.to_string(), ts)
            }
        }
    };
    if me.is_some_and(|me| me.as_str() == m.sender) {
        return None;
    }
    if let Ok(uid) = UserId::parse(&m.sender)
        && let Ok(Some(member)) = room.get_member_no_sync(&uid).await
    {
        m.sender_name = member.display_name().map(str::to_string);
    }
    Some(m)
}

enum Restart {
    /// Start over with a new device.
    Fresh(String),
    /// Try again with the same device.
    Later(String),
}

fn later(e: impl std::fmt::Display) -> Restart {
    Restart::Later(e.to_string())
}

fn invalid_token(e: &matrix_sdk::Error) -> bool {
    matches!(e.client_api_error_kind(), Some(ErrorKind::UnknownToken { .. }))
}

/// Read Matrix until the plugin stops: each failure is reported and tried again.
pub async fn run(shared: Arc<Shared>) {
    if let Err(e) = private_dir(&shared.dir) {
        shared.health.lock().await.sync_error = Some(format!("its state directory {}: {e}", shared.dir.display()));
        tracing::error!(error = %e, dir = %shared.dir.display(), "the state directory can't be used");
        return;
    }
    loop {
        let r = read(&shared).await;
        *shared.client.lock().await = None;
        let why = match r {
            Ok(()) => "the sync stopped".to_string(),
            Err(Restart::Fresh(why)) => {
                // The device is gone (signed out elsewhere, or deleted): a new one needs a new crypto store.
                tracing::warn!(%why, "the Matrix device is no longer valid: signing in again with a new device");
                let _ = std::fs::remove_file(shared.dir.join("session.json"));
                let _ = std::fs::remove_dir_all(shared.dir.join("sdk"));
                why
            }
            Err(Restart::Later(why)) => why,
        };
        tracing::warn!(%why, "the Matrix reader stopped: trying again in 30s");
        {
            let mut h = shared.health.lock().await;
            h.sync_error = Some(why);
            h.signed_in = false;
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Sign in (or reuse the saved device), resolve the rooms, and sync until something fails.
async fn read(shared: &Shared) -> Result<(), Restart> {
    let cfg = &shared.cfg;
    let dir = &shared.dir;
    let client = sign_in(cfg, &shared.node, dir).await?;
    let me = client.user_id().map(ToOwned::to_owned).ok_or_else(|| later("signed in, but there's no user"))?;
    shared.health.lock().await.signed_in = true;
    *shared.client.lock().await = Some(client.clone());

    // Room IDs, by spec: the configured rooms and the heartbeat rooms.
    let mut ids: HashMap<String, OwnedRoomId> = HashMap::new();
    let specs = shared.rooms.iter().map(|r| r.spec.clone()).chain(cfg.heartbeats.iter().map(|h| h.room.clone()));
    for spec in specs {
        let id = resolve(&client, &spec).await?;
        ids.insert(spec, id);
    }
    *shared.ids.lock().await = ids.clone();
    let by_id: HashMap<OwnedRoomId, &config::Room> =
        shared.rooms.iter().filter_map(|r| ids.get(&r.spec).map(|id| (id.clone(), r))).collect();
    let beats: HashMap<OwnedRoomId, PathBuf> = cfg
        .heartbeats
        .iter()
        .filter_map(|h| ids.get(&h.room).map(|id| (id.clone(), expand(Path::new(&home()), &h.ping_url_file))))
        .collect();
    let mut last_ping: HashMap<OwnedRoomId, Instant> = HashMap::new();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(later)?;

    let mut filter = FilterDefinition::default();
    filter.room.timeline.limit = Some(UInt::from(TIMELINE_LIMIT));
    let settings = SyncSettings::default().timeout(SYNC_TIMEOUT).filter(Filter::FilterDefinition(filter));

    let mut pos = Position::load(dir);
    if pos.since.is_none() {
        // Start from now: what the rooms held before isn't published.
        let r = client.sync_once(settings.clone().token(SyncToken::NoToken)).await;
        let resp = r.map_err(|e| if invalid_token(&e) { Restart::Fresh(e.to_string()) } else { later(e) })?;
        pos.since = Some(resp.next_batch);
        let now = Timestamp::now().as_millisecond();
        for id in by_id.keys() {
            pos.newest.insert(id.to_string(), now);
        }
        pos.save(dir).map_err(later)?;
        tracing::info!("first sync: reading from now on");
    }

    tracing::info!(rooms = by_id.len(), heartbeats = beats.len(), "reading Matrix");
    loop {
        let since = pos.since.clone().unwrap_or_default();
        let r = client.sync_once(settings.clone().token(since)).await;
        let resp = match r {
            Ok(r) => r,
            Err(e) if invalid_token(&e) => return Err(Restart::Fresh(e.to_string())),
            Err(e) => return Err(later(e)),
        };

        let mut events: Vec<Event> = vec![];
        let timelines = resp
            .rooms
            .joined
            .iter()
            .map(|(id, u)| (id, &u.timeline))
            .chain(resp.rooms.left.iter().map(|(id, u)| (id, &u.timeline)));
        for (id, timeline) in timelines {
            let Some(room) = client.get_room(id) else { continue };
            if let Some(file) = beats.get(id) {
                if !timeline.events.is_empty() {
                    let due = last_ping.get(id).is_none_or(|t| t.elapsed() >= PING_EVERY);
                    if due {
                        last_ping.insert(id.clone(), Instant::now());
                        let e = ping(&http, file).await.err().map(|e| format!("the heartbeat for {id} failed: {e}"));
                        shared.health.lock().await.heartbeat_error = e;
                    }
                }
                continue;
            }
            let Some(conf) = by_id.get(id) else { continue };
            let newest = pos.newest.get(id.as_str()).copied();
            let mut evs: Vec<TimelineEvent> = vec![];
            if timeline.limited
                && let (Some(prev), Some(after)) = (&timeline.prev_batch, newest)
            {
                evs.extend(backfill(&room, prev, after).await.map_err(later)?);
            }
            evs.extend(timeline.events.iter().cloned());
            for ev in &evs {
                if let Some(m) = message(&room, ev, Some(&me)).await {
                    let n = pos.newest.entry(id.to_string()).or_insert(0);
                    *n = (*n).max(m.ts_ms);
                    events.push(render::event(conf, &m, shared.reply_with.as_deref()));
                }
            }
        }
        if !events.is_empty() {
            let n = events.len() as u64;
            shared.outbox.lock().await.push(events);
            shared.health.lock().await.read += n;
            shared.ready.notify_one();
        }
        // Only now, with this sync's events saved, does the position move past them.
        pos.since = Some(resp.next_batch);
        pos.save(dir).map_err(later)?;
        {
            let mut h = shared.health.lock().await;
            h.last_sync = Some(Timestamp::now());
            h.last_sync_at = Some(Instant::now());
            h.sync_error = None;
        }
        membership(shared, &client, &ids).await;
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

async fn sign_in(cfg: &Config, node: &str, dir: &Path) -> Result<Client, Restart> {
    let client = Client::builder()
        .homeserver_url(&cfg.homeserver)
        .sqlite_store(dir.join("sdk"), None)
        .build()
        .await
        .map_err(later)?;
    let session_path = dir.join("session.json");
    let saved: Option<MatrixSession> = std::fs::read(&session_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    match saved {
        Some(session) => client.restore_session(session).await.map_err(later)?,
        None => {
            let password = read_secret_file(&expand(Path::new(&home()), &cfg.password_file)).map_err(later)?;
            let name = cfg.device_name.clone().unwrap_or_else(|| format!("Fleetlayer ({node})"));
            client
                .matrix_auth()
                .login_username(&cfg.user, &password)
                .initial_device_display_name(&name)
                .send()
                .await
                .map_err(|e| later(format!("signing in as {}: {e}", cfg.user)))?;
            let session = client.matrix_auth().session().ok_or_else(|| later("signed in, but there's no session"))?;
            write_private(&session_path, &serde_json::to_vec(&session).expect("sessions serialise")).map_err(later)?;
            tracing::info!(user = %cfg.user, device = %session.meta.device_id, "signed in to Matrix with a new device");
        }
    }
    if !client.encryption().backups().are_enabled().await {
        tracing::info!(
            "room key backup is off for this device (the plugin doesn't set one up): keys stay in its own store"
        );
    }
    Ok(client)
}

async fn resolve(client: &Client, spec: &str) -> Result<OwnedRoomId, Restart> {
    if spec.starts_with('!') {
        return OwnedRoomId::try_from(spec).map_err(|e| later(format!("{spec}: {e}")));
    }
    let alias = OwnedRoomAliasId::try_from(spec).map_err(|e| later(format!("{spec}: {e}")))?;
    let r = client.resolve_room_alias(&alias).await.map_err(|e| later(format!("resolving {spec}: {e}")))?;
    Ok(r.room_id)
}

/// Fetch a room's events back from `from` to the newest message already seen (`after_ms`), oldest first. A message at
/// that same millisecond is fetched again; the control plane drops it as a repeat.
async fn backfill(room: &Room, from: &str, after_ms: i64) -> Result<Vec<TimelineEvent>, matrix_sdk::Error> {
    let mut out = vec![];
    let mut token = Some(from.to_string());
    while let Some(t) = token.take() {
        let mut opts = MessagesOptions::backward().from(t.as_str());
        opts.limit = UInt::from(100u32);
        let page = room.messages(opts).await?;
        let mut done = page.chunk.is_empty();
        for ev in page.chunk {
            let ts = ev.timestamp.map_or(i64::MAX, |t| i64::try_from(u64::from(t.0)).unwrap_or(i64::MAX));
            if ts < after_ms {
                done = true;
                break;
            }
            out.push(ev);
        }
        if out.len() >= BACKFILL_MAX {
            tracing::warn!(room = %room.room_id(), "a gap of over {BACKFILL_MAX} events: the oldest aren't published");
            break;
        }
        if !done {
            token = page.end;
        }
    }
    tracing::info!(room = %room.room_id(), events = out.len(), "filled a gap in a busy room's timeline");
    out.reverse();
    Ok(out)
}

/// Record each configured room's membership, accept an invitation to one, and try joining one the bot isn't in now and
/// then (a public room, or one it was invited to).
async fn membership(shared: &Shared, client: &Client, ids: &HashMap<String, OwnedRoomId>) {
    let mut states = BTreeMap::new();
    for (spec, id) in ids {
        let state = client.get_room(id).map(|r| r.state());
        let word = state.map_or("not joined", state_word);
        states.insert(spec.clone(), word.to_string());
        if matches!(state, Some(RoomState::Joined) | Some(RoomState::Banned)) {
            continue;
        }
        let due = {
            let mut h = shared.health.lock().await;
            let due = h.join_tried.get(id.as_str()).is_none_or(|t| t.elapsed() >= JOIN_EVERY);
            if due {
                h.join_tried.insert(id.to_string(), Instant::now());
            }
            due
        };
        if !due {
            continue;
        }
        match client.join_room_by_id(id).await {
            Ok(_) => tracing::info!(room = %spec, "joined"),
            Err(e) => tracing::warn!(room = %spec, error = %e, "can't join"),
        }
    }
    shared.health.lock().await.rooms = states;
}

async fn ping(http: &reqwest::Client, file: &Path) -> Result<(), String> {
    let url = read_secret_file(file)?;
    match http.get(url.trim()).send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("the ping URL answered {}", r.status())),
        Err(e) => Err(format!("the ping failed: {}", e.without_url())),
    }
}

/// `fleetlayer-matrix check`: sign in (or reuse the device), resolve every room, sync once and say what the bot is in.
/// Publishes nothing and doesn't move the position. Stop the plugin first: two readers mustn't share a store.
pub async fn check(cfg: &Config, node: &str, dir: &Path) -> anyhow::Result<()> {
    let rooms = cfg.check().map_err(anyhow::Error::msg)?;
    private_dir(dir)?;
    let client = match sign_in(cfg, node, dir).await {
        Ok(c) => c,
        Err(Restart::Fresh(e) | Restart::Later(e)) => anyhow::bail!("{e}"),
    };
    println!("Signed in as {} (device {}).", cfg.user, client.device_id().map(|d| d.to_string()).unwrap_or_default());
    // The SDK's own position, not the plugin's: nothing is published from this sync.
    client.sync_once(SyncSettings::default().timeout(Duration::from_secs(10))).await?;
    for r in &rooms {
        match resolve(&client, &r.spec).await {
            Ok(id) => {
                let state = client.get_room(&id).map_or("not joined", |x| state_word(x.state()));
                println!(
                    "  {} → channel {} ({id}): {state}{}",
                    r.spec,
                    r.channel,
                    if r.post { "" } else { ", no posting" }
                );
            }
            Err(Restart::Fresh(e) | Restart::Later(e)) => println!("  {}: {e}", r.spec),
        }
    }
    for h in &cfg.heartbeats {
        let file = expand(Path::new(&home()), &h.ping_url_file);
        let url = read_secret_file(&file).map(|_| "ping URL readable".to_string()).unwrap_or_else(|e| e);
        println!("  heartbeat {}: {url}", h.room);
    }
    Ok(())
}
