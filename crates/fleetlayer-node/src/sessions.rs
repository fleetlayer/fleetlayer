//! The node's view of its sessions: what each session-manager and harness plugin reports, merged by session ID, with
//! changes broadcast to whoever watches (the control plane link, `fleetlayer sessions --watch`).

use std::collections::BTreeMap;

use fleetlayer_protocol::acp::{SessionInfo, SessionInfoUpdate};
use fleetlayer_protocol::ext::{self, How, SessionEnded, SessionMeta, Status, meta_key};
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
}

impl Default for Registry {
    fn default() -> Self {
        Registry { sessions: RwLock::default(), changes: broadcast::channel(1024).0 }
    }
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

    /// A plugin's full list: sessions it no longer lists are dropped from its role (and dropped entirely when no
    /// plugin reports them), except ended ones, which stay listed for a while.
    pub async fn replace(&self, plugin: &str, role: Role, list: Vec<SessionInfo>) {
        let now = Timestamp::now();
        let mut sessions = self.sessions.write().await;
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
                    entry.meta = meta;
                    entry.meta.deliverable = entry.meta.deliverable.or(deliverable);
                    entry.meta.prompt = prompt;
                    entry.meta.background = background;
                    entry.ended = None;
                }
                Role::Harness => {
                    entry.harness_plugin = meta.deliverable.unwrap_or(false).then(|| plugin.to_string());
                    entry.meta.deliverable = meta.deliverable;
                    // The harness's list is the whole truth about its prompt: absent there means none now.
                    entry.meta.prompt = meta.prompt.clone().flatten().map(Some);
                    if meta.background.is_some() {
                        entry.meta.background = meta.background.clone();
                    }
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
        s.meta.merge(&meta);
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
        let mut sessions = self.sessions.write().await;
        let Some(s) = sessions.get_mut(&e.session_id) else { return };
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
        let sessions = self.sessions.read().await;
        if let Some(s) = sessions.get(name) {
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
