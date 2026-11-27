//! "Since your last message": a short catch-up of what a session did after the user's last real message, for the
//! transcript viewer's header.
//!
//! The anchor is the last `Human` event the noise filter keeps, so wake-ups, task notifications, plugin messages and
//! compaction summaries never count as "your message". Compaction doesn't remove lines from the transcript, so a
//! message from before the latest compaction is found too. The catch-up is cached against the transcript's size: the
//! cached text is served at once, marked `stale` while a new one is written.

use jiff::Timestamp;

use super::call::{CallError, CallRequest, parse_catchup};
use super::condense;
use super::engine::{Engine, Stop};
use super::prompt::{self, SessionInfo};
use super::transcript::{self, Agent, Event, Kind, Source};
use fleetlayer_protocol::summary::{Catchup, CatchupAnchor};

/// The user's last real message and everything after it.
pub fn split_at_anchor(events: &[Event]) -> (Option<&Event>, &[Event]) {
    match events.iter().rposition(|e| e.kind == Kind::Human) {
        Some(i) => (Some(&events[i]), &events[i + 1..]),
        None => (None, events),
    }
}

impl Engine {
    /// The session's main transcript: the most recently modified one.
    fn main_transcript(&self, session_id: &str, agent: Agent) -> Option<Source> {
        self.locate(session_id, agent).into_iter().max_by_key(|s| transcript::modified(&s.path))
    }

    /// The cached catch-up, marked stale when the transcript has grown since it was written; None if there's none.
    /// The second value says whether a new one should be written.
    pub fn cached_catchup(&self, session_id: &str, agent: Agent) -> (Option<Catchup>, bool) {
        let size =
            self.main_transcript(session_id, agent).and_then(|s| std::fs::metadata(&s.path).ok()).map(|m| m.len());
        let cached = self.state.lock().expect("state lock").catchups.get(session_id).cloned();
        match (cached, size) {
            (Some(mut c), Some(size)) => {
                c.stale = c.offset != size;
                let regenerate = c.stale;
                (Some(c), regenerate)
            }
            (None, Some(_)) => (None, true),
            (c, None) => (c, false),
        }
    }

    /// Write a new catch-up and cache it. Nothing after the message needs no model call.
    pub async fn write_catchup(
        &self,
        session_id: &str,
        agent: Agent,
        mut info: SessionInfo,
    ) -> Result<Option<Catchup>, Stop> {
        let Some(source) = self.main_transcript(session_id, agent) else {
            return Ok(None);
        };
        let (src, home) = (source.clone(), self.config.roots.home.clone());
        let res = match tokio::task::spawn_blocking(move || transcript::read(&src, 0, &home)).await.expect("read task")
        {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!("summaries: catch-up for {session_id}: {e}");
                return Ok(None);
            }
        };
        if res.skip.is_some() {
            return Ok(None);
        }
        let (anchor_event, after) = split_at_anchor(&res.events);
        let tz = &self.config.tz;
        let anchor = anchor_event.map(|e| CatchupAnchor { uuid: e.uuid.clone(), at: e.ts, end: e.end });
        info.session_id = session_id.to_owned();
        info.agent = Some(agent);
        if info.dir.is_none() {
            info.dir = res.cwd.clone();
        }
        let text = if after.is_empty() {
            "Nothing yet since your last message.".to_owned()
        } else {
            let account = match self.accounts_pick().await? {
                Some(a) => a,
                None => return Err(Stop::NoAccount),
            };
            let page = self.state.lock().expect("state lock").sessions.get(session_id).map(|s| s.page.clone());
            let (message, at) = match anchor_event {
                Some(e) => (condense::clip(&e.text, 2500), condense::stamp(e.ts, tz)),
                None => ("(none in this transcript)".to_owned(), "never".to_owned()),
            };
            let user_prompt = prompt::build_catchup_prompt(
                &info,
                page.as_deref(),
                &message,
                &at,
                &condense::condense(after, self.config.budget, tz),
            );
            let req = CallRequest {
                system_prompt: prompt::catchup_system(&self.config.user_name),
                schema: prompt::CATCHUP_SCHEMA.to_owned(),
                prompt: user_prompt,
                account: account.clone(),
            };
            match self.model.call(req).await.and_then(|(v, _)| parse_catchup(&v)) {
                Ok(text) => text,
                Err(CallError::Limit(m)) => {
                    self.mark_limited(&account);
                    return Err(Stop::Limit(m));
                }
                Err(e) => {
                    tracing::warn!("summaries: catch-up for {session_id} failed: {e}");
                    return Ok(None);
                }
            }
        };
        let catchup = Catchup {
            session_id: session_id.to_owned(),
            anchor,
            text,
            generated_at: Timestamp::now(),
            offset: res.end,
            stale: false,
        };
        self.state.lock().expect("state lock").catchups.insert(session_id.to_owned(), catchup.clone());
        self.save();
        Ok(Some(catchup))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, text: &str, end: u64) -> Event {
        Event { kind, text: text.into(), ts: Timestamp::UNIX_EPOCH, end, uuid: Some(format!("u{end}")) }
    }

    #[test]
    fn anchor_is_the_last_human_message() {
        let events = [
            ev(Kind::Human, "one", 1),
            ev(Kind::Assistant, "a", 2),
            ev(Kind::Human, "two", 3),
            ev(Kind::Peer, "p", 4),
            ev(Kind::Tool, "t", 5),
        ];
        let (anchor, after) = split_at_anchor(&events);
        assert_eq!(anchor.unwrap().uuid.as_deref(), Some("u3"));
        assert_eq!(after.len(), 2);
        let (anchor, after) = split_at_anchor(&events[3..]);
        assert!(anchor.is_none());
        assert_eq!(after.len(), 2);
    }
}
