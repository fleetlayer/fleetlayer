//! The node's view of its sessions: what each session-manager and harness plugin reports, merged by session ID, with
//! changes broadcast to whoever watches (the control plane link, `fleetlayer sessions --watch`).

use std::collections::BTreeMap;

use fleetlayer_protocol::acp::{SessionInfo, SessionInfoUpdate};
use fleetlayer_protocol::ext::{self, How, SessionEnded, SessionMeta, Status, Turn, TurnCause, meta_key};
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use tokio::sync::{RwLock, broadcast};

/// How long an ended session stays listed (as not running) before it's dropped.
const KEEP_ENDED: SignedDuration = SignedDuration::from_hours(1);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub session_id: String,
    /// The session-manager plugin that runs it.
    pub manager: Option<String>,
    /// A harness plugin that can deliver into it through the agent's own channel.
    pub harness_plugin: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<Timestamp>,
    pub meta: SessionMeta,
    pub ended: Option<Ended>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ended {
    pub at: Timestamp,
    pub how: How,
}

impl Session {
    pub fn running(&self) -> bool {
        self.ended.is_none() && self.meta.running.unwrap_or(true)
    }
    pub fn status(&self) -> Status {
        self.meta.status.unwrap_or_default()
    }
    pub fn tab(&self) -> Option<&str> {
        self.meta.tab.as_deref()
    }
}

/// What changed, for watchers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    /// Everything may have changed: read the whole list again.
    Reset,
    Updated {
        session: Box<Session>,
    },
    Removed {
        session_id: String,
    },
}

/// Which role a plugin reports a session in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Manager,
    Harness,
}

pub struct Registry {
    sessions: RwLock<BTreeMap<String, Session>>,
    changes: broadcast::Sender<Change>,
    /// Sessions a session manager can't name (`@<address>`) that a harness plugin named by their address: the unnamed
    /// ID, then the harness's ID and the plugin that said so. A harness whose agent the manager doesn't recognise (Gemini
    /// CLI in herdr) knows its pane from the agent's environment, so the two reports join up.
    aliases: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry { sessions: RwLock::default(), changes: broadcast::channel(1024).0, aliases: Default::default() }
    }
}

/// A typed prompt this soon after a delivery or an act of the node's is that delivery typed in, not a person.
const SAME_TURN: SignedDuration = SignedDuration::from_secs(10);

/// Take `incoming` as the session's latest turn signal unless it's older, or it's the harness seeing as typed what
/// the node itself just delivered or typed.
fn take_turn(current: &mut Option<Turn>, incoming: Option<Turn>) {
    let Some(new) = incoming else { return };
    if let Some(cur) = current {
        if new.at < cur.at {
            return;
        }
        let typed_echo = new.cause == TurnCause::Person
            && new.detail.as_deref() == Some("typed")
            && cur.cause != TurnCause::Unknown
            && new.at.duration_since(cur.at) < SAME_TURN;
        if typed_echo {
            return;
        }
    }
    *current = Some(new);
}

fn meta_of(info: &SessionInfo) -> SessionMeta {
    ext::get_meta(info.meta.as_ref(), meta_key::SESSION).ok().flatten().unwrap_or_default()
}

impl Registry {
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    pub async fn all(&self) -> Vec<Session> {
        self.sessions.read().await.values().cloned().collect()
    }

    pub async fn get(&self, id: &str) -> Option<Session> {
        self.sessions.read().await.get(id).cloned()
    }

    fn emit(&self, c: Change) {
        let _ = self.changes.send(c);
    }

    /// The ID a session goes by: a harness's own for an unnamed one it named.
    fn named(&self, id: &str) -> String {
        if !id.starts_with('@') {
            return id.to_string();
        }
        self.aliases.lock().expect("aliases").get(id).map(|(real, _)| real.clone()).unwrap_or_else(|| id.to_string())
    }

    /// Take a harness plugin's names for unnamed sessions (its entries that carry an address), and move each unnamed
    /// session listed so far to its real ID.
    fn take_names(&self, plugin: &str, list: &[SessionInfo], sessions: &mut BTreeMap<String, Session>) {
        let mut aliases = self.aliases.lock().expect("aliases");
        aliases.retain(|_, (_, p)| p != plugin);
        for info in list {
            let meta = meta_of(info);
            let Some(address) = meta.address.filter(|_| !info.session_id.starts_with('@')) else { continue };
            let unnamed = format!("@{address}");
            // Never another harness's session: not a pane the manager says runs another agent, and not an ID another
            // harness plugin already reports.
            let harness = meta.harness.as_deref();
            let other_agent =
                |s: &Session| s.meta.harness.as_deref().is_some_and(|h| harness.is_some_and(|mine| mine != h));
            if sessions.get(&unnamed).is_some_and(other_agent)
                || sessions
                    .get(&info.session_id)
                    .is_some_and(|s| other_agent(s) || s.harness_plugin.as_deref().is_some_and(|p| p != plugin))
            {
                continue;
            }
            aliases.insert(unnamed.clone(), (info.session_id.clone(), plugin.to_string()));
            let Some(mut from) = sessions.remove(&unnamed) else { continue };
            match sessions.get_mut(&info.session_id) {
                Some(to) => {
                    to.manager = from.manager.take();
                    to.cwd = from.cwd.take().or(to.cwd.take());
                    to.title = from.title.take().or(to.title.take());
                    to.updated_at = from.updated_at.or(to.updated_at);
                    let (prompt, background, deliverable, turn) =
                        (to.meta.prompt.take(), to.meta.background.take(), to.meta.deliverable, to.meta.turn.take());
                    to.meta = from.meta;
                    take_turn(&mut to.meta.turn, turn);
                    (to.meta.prompt, to.meta.background, to.meta.deliverable) = (prompt, background, deliverable);
                    to.meta.named = Some(true);
                    to.ended = from.ended;
                }
                None => {
                    from.session_id = info.session_id.clone();
                    from.meta.named = Some(true);
                    sessions.insert(info.session_id.clone(), from);
                }
            }
        }
    }

    /// A plugin's full list: sessions it no longer lists are dropped from its role (and dropped entirely when no
    /// plugin reports them), except ended ones, which stay listed for a while.
    pub async fn replace(&self, plugin: &str, role: Role, mut list: Vec<SessionInfo>) {
        let now = Timestamp::now();
        let mut sessions = self.sessions.write().await;
        match role {
            Role::Manager => list.iter_mut().for_each(|i| i.session_id = self.named(&i.session_id)),
            Role::Harness => self.take_names(plugin, &list, &mut sessions),
        }
        let listed: std::collections::HashSet<String> = list.iter().map(|i| i.session_id.clone()).collect();
        for info in list {
            let meta = meta_of(&info);
            let entry = sessions.entry(info.session_id.clone()).or_insert_with(|| Session {
                session_id: info.session_id.clone(),
                manager: None,
                harness_plugin: None,
                cwd: None,
                title: None,
                updated_at: None,
                meta: SessionMeta::default(),
                ended: None,
            });
            match role {
                Role::Manager => {
                    entry.manager = Some(plugin.into());
                    entry.cwd = info.cwd.or(entry.cwd.take());
                    entry.title = info.title.or(entry.title.take());
                    entry.updated_at = info.updated_at.or(entry.updated_at);
                    let deliverable = entry.meta.deliverable;
                    // What the harness reports (a blocked prompt, background work) isn't the manager's to replace.
                    let (prompt, background) = (entry.meta.prompt.take(), entry.meta.background.take());
                    let mut turn = entry.meta.turn.take();
                    take_turn(&mut turn, meta.turn.clone());
                    entry.meta = meta;
                    entry.meta.turn = turn;
                    if entry.meta.named == Some(false) && !entry.session_id.starts_with('@') {
                        entry.meta.named = Some(true); // a harness named it (`take_names`)
                    }
                    entry.meta.deliverable = entry.meta.deliverable.or(deliverable);
                    entry.meta.prompt = prompt;
                    entry.meta.background = background;
                    entry.ended = None;
                }
                Role::Harness => {
                    // A session another harness plugin reports, or that its manager says runs another agent, isn't
                    // this plugin's to claim.
                    let other_plugin = entry.harness_plugin.as_deref().is_some_and(|p| p != plugin);
                    let other_agent = meta.harness.as_deref().is_some_and(|h| {
                        entry.meta.harness.as_deref().is_some_and(|known| known != h)
                            && (entry.manager.is_some() || entry.harness_plugin.is_some())
                    });
                    if other_plugin || other_agent {
                        tracing::warn!(session = %info.session_id, "{plugin} lists a session another harness has");
                        continue;
                    }
                    // The harness plugin knows this session (its transcript, its prompts) whether or not it has a
                    // channel into it; one without (`deliverable: false`) answers a delivery with NO_CHANNEL, and the
                    // node types the message in.
                    entry.harness_plugin = Some(plugin.to_string());
                    entry.meta.deliverable = meta.deliverable;
                    // The harness's list is the whole truth about its prompt: absent there means none now.
                    entry.meta.prompt = meta.prompt.clone().flatten().map(Some);
                    if meta.background.is_some() {
                        entry.meta.background = meta.background.clone();
                    }
                    take_turn(&mut entry.meta.turn, meta.turn.clone());
                    if entry.manager.is_none() {
                        entry.cwd = info.cwd.or(entry.cwd.take());
                        entry.meta.harness = meta.harness.or(entry.meta.harness.take());
                    }
                }
            }
        }
        sessions.retain(|id, s| {
            if listed.contains(id) {
                return true;
            }
            match role {
                Role::Manager if s.manager.as_deref() == Some(plugin) && s.ended.is_none() => s.manager = None,
                Role::Harness if s.harness_plugin.as_deref() == Some(plugin) => {
                    s.harness_plugin = None;
                    s.meta.deliverable = None;
                    s.meta.prompt = None;
                }
                _ => {}
            }
            let recently_ended = s.ended.as_ref().is_some_and(|e| now.duration_since(e.at) < KEEP_ENDED);
            s.manager.is_some() || s.harness_plugin.is_some() || recently_ended
        });
        drop(sessions);
        self.emit(Change::Reset);
    }

    /// A plugin stopped: forget what only it reported.
    pub async fn plugin_gone(&self, plugin: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, s| {
            if s.manager.as_deref() == Some(plugin) {
                s.manager = None;
            }
            if s.harness_plugin.as_deref() == Some(plugin) {
                s.harness_plugin = None;
                s.meta.deliverable = None;
            }
            s.manager.is_some() || s.harness_plugin.is_some() || s.ended.is_some()
        });
        drop(sessions);
        self.emit(Change::Reset);
    }

    /// A `session_info_update`. Returns false when the session is unknown and has too little to list (the caller
    /// then reads the plugin's list again).
    pub async fn update(&self, plugin: &str, session_id: &str, u: &SessionInfoUpdate) -> bool {
        let meta: SessionMeta = ext::get_meta(u.meta.as_ref(), meta_key::SESSION).ok().flatten().unwrap_or_default();
        let session_id = &self.named(session_id);
        let mut sessions = self.sessions.write().await;
        let s = match sessions.get_mut(session_id) {
            Some(s) => s,
            None if meta.harness.is_some() && meta.address.is_some() => {
                sessions.entry(session_id.into()).or_insert_with(|| Session {
                    session_id: session_id.into(),
                    manager: Some(plugin.into()),
                    harness_plugin: None,
                    cwd: None,
                    title: None,
                    updated_at: None,
                    meta: SessionMeta::default(),
                    ended: None,
                })
            }
            None => return false,
        };
        let mut meta = meta;
        let incoming = meta.turn.take();
        s.meta.merge(&meta);
        take_turn(&mut s.meta.turn, incoming);
        if u.title.is_some() {
            s.title = u.title.clone();
        }
        if u.updated_at.is_some() {
            s.updated_at = u.updated_at;
        }
        let session = Box::new(s.clone());
        drop(sessions);
        self.emit(Change::Updated { session });
        true
    }

    /// Something the node did that starts a turn in session `session_id`: a delivery, an act typed in for someone.
    pub async fn note_turn(&self, session_id: &str, turn: Turn) {
        let mut sessions = self.sessions.write().await;
        let Some(s) = sessions.get_mut(session_id) else { return };
        let before = s.meta.turn.clone();
        take_turn(&mut s.meta.turn, Some(turn));
        if s.meta.turn == before {
            return;
        }
        let session = Box::new(s.clone());
        drop(sessions);
        self.emit(Change::Updated { session });
    }

    /// The options of the prompt `prompt_id` a session is blocked on, read from elsewhere (its screen).
    pub async fn set_prompt_options(
        &self,
        session_id: &str,
        prompt_id: &str,
        options: Vec<fleetlayer_protocol::ext::PromptOption>,
        from: &str,
    ) {
        let mut sessions = self.sessions.write().await;
        let Some(s) = sessions.get_mut(session_id) else { return };
        let Some(Some(p)) = s.meta.prompt.as_mut() else { return };
        if p.id != prompt_id {
            return;
        }
        p.options = options;
        p.options_from = Some(from.into());
        let session = Box::new(s.clone());
        drop(sessions);
        self.emit(Change::Updated { session });
    }

    pub async fn ended(&self, e: &SessionEnded) {
        let id = self.named(&e.session_id);
        let mut sessions = self.sessions.write().await;
        let Some(s) = sessions.get_mut(&id) else { return };
        s.ended = Some(Ended { at: e.at, how: e.how.clone() });
        s.meta.running = Some(false);
        if let How::AgentExited { final_status: Some(st), .. } = &e.how {
            s.meta.status = Some(*st);
        }
        let session = Box::new(s.clone());
        drop(sessions);
        self.emit(Change::Updated { session });
    }

    pub async fn renamed(&self, from: &str, to: &str) {
        let mut sessions = self.sessions.write().await;
        let Some(mut s) = sessions.remove(from) else { return };
        s.session_id = to.into();
        s.meta.named = Some(true);
        let session = Box::new(s.clone());
        sessions.insert(to.into(), s);
        drop(sessions);
        self.emit(Change::Removed { session_id: from.into() });
        self.emit(Change::Updated { session });
    }

    /// Running sessions matching a human address (`tab`) or a session ID.
    pub async fn find(&self, name: &str) -> Vec<Session> {
        let named = self.named(name);
        let sessions = self.sessions.read().await;
        if let Some(s) = sessions.get(&named) {
            return vec![s.clone()];
        }
        sessions.values().filter(|s| s.running() && s.tab() == Some(name)).cloned().collect()
    }

    /// The running session at a session manager's address.
    pub async fn by_address(&self, address: &str) -> Option<Session> {
        self.sessions.read().await.values().find(|s| s.running() && s.meta.address.as_deref() == Some(address)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleetlayer_protocol::acp::SessionInfoTag;
    use serde_json::json;

    fn info(id: &str, tab: &str, extra: serde_json::Value) -> SessionInfo {
        let mut meta = json!({"harness":"claude","address":format!("p-{id}"),"tab":tab,"status":"idle","running":true});
        meta.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(json!({"sessionId":id,"title":tab,"_meta":{"fleetlayer.dev/session":meta}})).unwrap()
    }

    #[tokio::test]
    async fn a_typed_prompt_right_after_a_delivery_is_the_delivery() {
        let r = Registry::default();
        r.replace("herdr", Role::Manager, vec![info("a", "a", json!({}))]).await;
        let t0 = Timestamp::now();
        let turn = |cause, secs: i64, detail: &str| Turn {
            cause,
            at: t0 + SignedDuration::from_secs(secs),
            from: None,
            detail: Some(detail.into()),
        };
        r.note_turn("a", turn(TurnCause::Message, 0, "message")).await;
        // The harness sees the typed-in delivery as typed: still the message.
        let u = SessionInfoUpdate {
            session_update: SessionInfoTag::SessionInfoUpdate,
            title: None,
            updated_at: None,
            meta: Some(ext::meta_with(
                meta_key::SESSION,
                &SessionMeta { turn: Some(turn(TurnCause::Person, 2, "typed")), ..Default::default() },
            )),
        };
        r.update("claude-code", "a", &u).await;
        assert_eq!(r.get("a").await.unwrap().meta.turn.unwrap().cause, TurnCause::Message);
        // Later, a person typing is theirs; an older signal never replaces a newer one.
        r.note_turn("a", turn(TurnCause::Person, 30, "typed")).await;
        r.note_turn("a", turn(TurnCause::Message, 20, "message")).await;
        assert_eq!(r.get("a").await.unwrap().meta.turn.unwrap().cause, TurnCause::Person);
        // A manager's list keeps it.
        r.replace("herdr", Role::Manager, vec![info("a", "a", json!({}))]).await;
        assert_eq!(r.get("a").await.unwrap().meta.turn.unwrap().cause, TurnCause::Person);
    }

    #[tokio::test]
    async fn a_wrong_name_gets_running_tabs_that_contain_it() {
        let r = Registry::default();
        r.replace(
            "herdr",
            Role::Manager,
            vec![
                info("a", "quietdeck triage", json!({})),
                info("b", "Triage Notes", json!({})),
                info("c", "ci", json!({})),
                info("d", "old triage", json!({"running": false})),
            ],
        )
        .await;
        let all = r.all().await;
        assert_eq!(
            crate::deliver::suggest(&all, "triage", "agents-triage"),
            vec!["Triage Notes@agents-triage", "quietdeck triage@agents-triage"],
            "any case, running only"
        );
        assert!(crate::deliver::suggest(&all, "billing", "n").is_empty());
    }

    #[tokio::test]
    async fn a_managers_list_replaces_its_sessions_and_a_harness_adds_delivery() {
        let r = Registry::default();
        r.replace("herdr", Role::Manager, vec![info("a", "planning", json!({})), info("b", "ci", json!({}))]).await;
        r.replace("claude", Role::Harness, vec![info("a", "", json!({"deliverable": true}))]).await;
        let a = r.get("a").await.unwrap();
        assert_eq!(a.tab(), Some("planning"), "the harness doesn't overwrite the manager's fields");
        assert_eq!(a.harness_plugin.as_deref(), Some("claude"));

        r.replace("herdr", Role::Manager, vec![info("a", "planning", json!({}))]).await;
        assert!(r.get("b").await.is_none(), "b is no longer listed by anyone");
        assert_eq!(
            r.get("a").await.unwrap().harness_plugin.as_deref(),
            Some("claude"),
            "the manager's relist keeps the harness"
        );
    }

    #[tokio::test]
    async fn a_harness_names_an_unnamed_session_by_its_address() {
        let r = Registry::default();
        let unnamed =
            |status: &str| info("@w1:p5", "gem", json!({"harness": "gemini", "address": "w1:p5", "status": status}));
        r.replace("herdr", Role::Manager, vec![unnamed("idle")]).await;
        r.replace(
            "gemini",
            Role::Harness,
            vec![info("g1", "", json!({"harness": "gemini", "address": "w1:p5", "deliverable": false}))],
        )
        .await;
        assert!(r.get("@w1:p5").await.is_none(), "moved to the harness's ID");
        let g = r.get("g1").await.unwrap();
        assert_eq!(
            (g.tab(), g.manager.as_deref(), g.harness_plugin.as_deref()),
            (Some("gem"), Some("herdr"), Some("gemini"))
        );
        assert_eq!(g.meta.named, Some(true));
        assert_eq!(r.find("@w1:p5").await.len(), 1, "still found by its old name");
        // The manager keeps listing it unnamed, and its updates and end land on the named session.
        r.replace("herdr", Role::Manager, vec![unnamed("working")]).await;
        assert!(r.get("@w1:p5").await.is_none());
        assert_eq!(r.get("g1").await.unwrap().status(), Status::Working);
        let e = SessionEnded { session_id: "@w1:p5".into(), at: Timestamp::now(), how: How::TabClosed };
        r.ended(&e).await;
        assert!(!r.get("g1").await.unwrap().running());
    }

    #[tokio::test]
    async fn a_harness_never_takes_another_harnesss_session() {
        let r = Registry::default();
        // herdr runs Claude Code in w1:p1; a gemini plugin claiming that pane is ignored.
        r.replace("herdr", Role::Manager, vec![info("@w1:p1", "c", json!({"harness": "claude", "address": "w1:p1"}))])
            .await;
        r.replace("gemini", Role::Harness, vec![info("g1", "", json!({"harness": "gemini", "address": "w1:p1"}))])
            .await;
        assert!(r.get("@w1:p1").await.is_some(), "the Claude pane stays as it was");
        // A claude session the Claude plugin reports can't be claimed by ID through another pane either.
        r.replace("claude", Role::Harness, vec![info("s1", "", json!({"deliverable": true}))]).await;
        r.replace("herdr", Role::Manager, vec![info("@w1:p2", "x", json!({"harness": "gemini", "address": "w1:p2"}))])
            .await;
        r.replace("gemini", Role::Harness, vec![info("s1", "", json!({"harness": "gemini", "address": "w1:p2"}))])
            .await;
        assert!(r.get("@w1:p2").await.is_some(), "not moved onto the Claude session");
        assert_eq!(r.get("s1").await.unwrap().meta.harness.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn a_harness_without_a_channel_still_owns_its_sessions_transcripts() {
        let r = Registry::default();
        r.replace("herdr", Role::Manager, vec![info("g", "gem", json!({}))]).await;
        r.replace("gemini", Role::Harness, vec![info("g", "", json!({"deliverable": false}))]).await;
        let g = r.get("g").await.unwrap();
        assert_eq!(g.harness_plugin.as_deref(), Some("gemini"), "asked for its transcript and prompts");
        assert_eq!(g.meta.deliverable, Some(false), "and still typed into");
        r.replace("gemini", Role::Harness, vec![]).await;
        assert_eq!(r.get("g").await.unwrap().harness_plugin, None);
    }

    #[tokio::test]
    async fn updates_merge_ended_sessions_stay_listed_and_names_resolve() {
        let r = Registry::default();
        r.replace("herdr", Role::Manager, vec![info("a", "planning", json!({})), info("b", "planning", json!({}))])
            .await;
        let u = SessionInfoUpdate {
            session_update: SessionInfoTag::SessionInfoUpdate,
            title: None,
            updated_at: None,
            meta: Some(ext::meta_with(meta_key::SESSION, &json!({"status":"working"}))),
        };
        assert!(r.update("herdr", "a", &u).await);
        assert_eq!(r.get("a").await.unwrap().status(), Status::Working);
        assert!(!r.update("herdr", "zzz", &u).await, "an unknown session with too little to list");

        assert_eq!(r.find("planning").await.len(), 2, "ambiguous");
        r.ended(&SessionEnded { session_id: "b".into(), at: Timestamp::now(), how: How::TabClosed }).await;
        assert_eq!(r.find("planning").await.len(), 1);
        r.replace("herdr", Role::Manager, vec![info("a", "planning", json!({}))]).await;
        assert!(!r.get("b").await.unwrap().running(), "an ended session stays listed, not running");
    }

    #[tokio::test]
    async fn an_unnamed_session_moves_to_its_real_id() {
        let r = Registry::default();
        r.replace("herdr", Role::Manager, vec![info("@p1", "new", json!({"named": false}))]).await;
        r.renamed("@p1", "real").await;
        assert!(r.get("@p1").await.is_none());
        assert_eq!(r.get("real").await.unwrap().meta.named, Some(true));
    }
}
