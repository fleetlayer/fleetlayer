//! A Matrix message as a Fleetlayer event. Plain functions, so they're tested without a homeserver.

use fleetlayer_protocol::ext::Event;
use jiff::Timestamp;
use serde_json::json;

use crate::config::Room;

/// The most of a message's text an event carries.
pub const MAX_TEXT: usize = 8_000;
/// The most of its first line the summary carries.
const MAX_SUMMARY: usize = 200;

/// One message read from a room.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub room_id: String,
    pub room_name: String,
    pub event_id: String,
    pub sender: String,
    /// The sender's display name in the room, when it has one.
    pub sender_name: Option<String>,
    pub text: String,
    /// When the homeserver received it, in milliseconds.
    pub ts_ms: i64,
    /// The event it replies to.
    pub reply_to: Option<String>,
    /// An encrypted message this device couldn't decrypt: `text` says so.
    pub undecryptable: bool,
}

/// What an encrypted message this device can't read becomes.
pub const UNDECRYPTABLE: &str = "(An encrypted message this device can't decrypt: the sender's app hasn't shared its \
                                 key with the bot's device. Read it in a Matrix app.)";

pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The event for `m`, read from `room`. `reply_with` is how an agent answers, when the plugin is shared as a tool.
pub fn event(room: &Room, m: &Message, reply_with: Option<&str>) -> Event {
    let who = m.sender_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| m.sender.clone());
    let first = m.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no text)");
    let summary = format!("{who} in {}: {}", m.room_name, clip(first, MAX_SUMMARY));
    let subject = if m.undecryptable { None } else { room.subject(&m.sender, &m.text).map(str::to_string) };
    let mut body = json!({
        "room": m.room_name,
        "roomId": m.room_id,
        "sender": m.sender,
        "eventId": m.event_id,
        "text": clip(&m.text, MAX_TEXT),
        "link": format!("https://matrix.to/#/{}/{}", m.room_id, m.event_id),
    });
    if let Some(n) = &m.sender_name {
        body["senderName"] = json!(n);
    }
    if let Some(r) = &m.reply_to {
        body["replyTo"] = json!(r);
    }
    if let (Some(tool), true) = (reply_with, room.post) {
        body["reply"] =
            json!(format!("fleetlayer tools call {tool} post room={} reply_to={} text='…'", room.channel, m.event_id));
    }
    Event {
        channel: room.channel.clone(),
        kind: if m.undecryptable { "matrix.undecryptable".into() } else { "matrix.message".into() },
        subject,
        at: Timestamp::from_millisecond(m.ts_ms).unwrap_or_else(|_| Timestamp::now()),
        // A copy decrypted later keeps the event's own key.
        dedupe_key: Some(if m.undecryptable {
            format!("matrix:{}:undecryptable", m.event_id)
        } else {
            format!("matrix:{}", m.event_id)
        }),
        summary,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn room() -> Room {
        let cfg: Config = serde_json::from_value(json!({
            "homeserver": "https://matrix.example.com",
            "user": "@fleetlayer:example.com",
            "password_file": "/p",
            "rooms": [{"room": "#infrastructure-alerts:example.com", "routes": [
                {"match": "(?i)backup|restic", "subject": "backup"},
                {"match": ".", "sender": "@gatus:example.com", "subject": "uptime"}
            ]}]
        }))
        .unwrap();
        cfg.check().unwrap().remove(0)
    }

    fn msg(sender: &str, text: &str) -> Message {
        Message {
            room_id: "!abc:example.com".into(),
            room_name: "Infrastructure alerts".into(),
            event_id: "$ev1".into(),
            sender: sender.into(),
            sender_name: Some("Alertmanager".into()),
            text: text.into(),
            ts_ms: 1_795_000_000_000,
            reply_to: None,
            undecryptable: false,
        }
    }

    #[test]
    fn a_message_becomes_an_event_on_its_rooms_channel() {
        let e = event(&room(), &msg("@am:example.com", "\n[FIRING:1] DiskFull on nas\nmore"), Some("matrix@server"));
        assert_eq!(e.channel, "infrastructure-alerts", "the alias's name");
        assert_eq!(e.kind, "matrix.message");
        assert_eq!(e.subject, None);
        assert_eq!(e.dedupe_key.as_deref(), Some("matrix:$ev1"));
        assert_eq!(e.summary, "Alertmanager in Infrastructure alerts: [FIRING:1] DiskFull on nas");
        assert_eq!(e.body["sender"], "@am:example.com");
        assert_eq!(e.body["link"], "https://matrix.to/#/!abc:example.com/$ev1");
        assert_eq!(e.at.as_millisecond(), 1_795_000_000_000);
        assert_eq!(
            e.body["reply"],
            "fleetlayer tools call matrix@server post room=infrastructure-alerts reply_to=$ev1 text='…'"
        );
    }

    #[test]
    fn routes_give_a_subject_the_first_match_wins() {
        let r = room();
        assert_eq!(event(&r, &msg("@am:example.com", "Restic backup failed"), None).subject.as_deref(), Some("backup"));
        assert_eq!(event(&r, &msg("@gatus:example.com", "BACKUP down"), None).subject.as_deref(), Some("backup"));
        assert_eq!(event(&r, &msg("@gatus:example.com", "web is down"), None).subject.as_deref(), Some("uptime"));
        assert_eq!(event(&r, &msg("@am:example.com", "web is down"), None).subject, None, "only from that sender");
    }

    #[test]
    fn an_undecryptable_message_is_still_told_under_a_key_of_its_own() {
        let mut m = msg("@am:example.com", UNDECRYPTABLE);
        m.undecryptable = true;
        let e = event(&room(), &m, None);
        assert_eq!(e.kind, "matrix.undecryptable");
        assert_eq!(e.dedupe_key.as_deref(), Some("matrix:$ev1:undecryptable"));
        assert!(e.body.get("reply").is_none(), "no reply line without a tool");
    }

    #[test]
    fn long_text_is_clipped_on_a_character_boundary() {
        let s = "é".repeat(5000);
        let c = clip(&s, 101);
        assert!(c.len() <= 104 && c.ends_with('…'));
        assert_eq!(clip("short", 10), "short");
        let e = event(&room(), &msg("@a:example.com", &"x".repeat(MAX_TEXT + 50)), None);
        assert!(e.body["text"].as_str().unwrap().len() <= MAX_TEXT + 3);
    }
}
