//! Session summaries: the records the summaries plugin writes. It keeps each as a Markdown file on its node and
//! publishes it to the control plane (`_fleetlayer.dev/summary/publish`), which stores it for search and the web UI.
//!
//! First written for Quietdeck (`quietdeck-types/src/summary.rs`) by the same author; the module that writes these
//! is a port of claude-summarizer (MIT, same author).

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::ext::Status as AgentStatus;

/// The agent program a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
}

/// The stretch of activity a record covers.
///
/// Live summaries are one point per local hour of activity: a later update in the same hour replaces the earlier one
/// (same `session_id`, same period). Old history (the import, and the daily sweep's catch-up on a long transcript) is
/// chunked by local day for the last 30 days and by ISO week before that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "snake_case")]
pub enum SummaryPeriod {
    /// The local hour of the activity's last real moment, floored: `2026-11-23T14`.
    Hour(String),
    /// A local day: `2026-11-23`.
    Day(String),
    /// An ISO week: `2026-W47`.
    Week(String),
}

/// Who wrote a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOrigin {
    /// Written by the node's summaries module.
    Node,
    /// Imported once from claude-summarizer's index.json and Markdown updates.
    ClaudeSummarizer,
}

/// One point on a session's timeline, with the session's three tiers as of that point.
///
/// Key: `session_id` + `period`. The control plane replaces a stored record with the same key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryRecord {
    pub session_id: String,
    pub agent: AgentKind,
    pub period: SummaryPeriod,
    /// The first and last real activity the record covers.
    pub activity_from: Timestamp,
    pub activity_to: Timestamp,
    pub written_at: Timestamp,
    /// The one-liner.
    pub sentence: String,
    /// One or two paragraphs.
    pub paragraph: String,
    /// Up to about a page, Markdown.
    pub page: String,
    /// The herdr tab label when the record was written, if the session had one.
    pub tab: Option<String>,
    /// The session's working directory.
    pub dir: Option<String>,
    pub origin: RecordOrigin,
}

/// The newest message from the person that the noise filter accepts as real (not a wake-up, tool output, compaction
/// or slash command). The chat view opens here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupAnchor {
    /// The transcript record's `uuid`, when it has one.
    pub uuid: Option<String>,
    pub at: Timestamp,
    /// Byte offset just past the message's line in the transcript.
    pub end: u64,
}

/// "Since your last message": a short account of what happened after the anchor, for the chat view's header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catchup {
    pub session_id: String,
    /// None when the transcript has no real message from the person.
    pub anchor: Option<CatchupAnchor>,
    pub text: String,
    pub generated_at: Timestamp,
    /// The transcript size `text` was written from.
    pub offset: u64,
    /// The transcript has grown since `offset`: this text is served now, and a new one is being written.
    pub stale: bool,
}

/// What the summaries module hands the node, which publishes each one to the control plane as a [`SummaryPush`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SummaryEvent {
    /// A new or replaced timeline point. Its tiers become the session's latest.
    Summary(SummaryRecord),
    /// The session's last real activity moved: sent after every successful call, `no_change` answers included, so
    /// the control plane's "last activity" stays right when the summary doesn't change.
    Activity { session_id: String, last_activity: Timestamp },
    /// A catch-up finished writing.
    Catchup(Catchup),
}

/// A summary event on its way from a node to the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryPush {
    pub node: String,
    pub event: SummaryEvent,
}

/// What the node tells the summaries module about a session: one per herdr status change, plus one for every
/// session the node finds when it starts (so the module can sweep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionSignal {
    /// herdr reported a status for the session (`pane.agent_status_changed`, or the snapshot at start-up).
    Status { session: SessionFacts, status: AgentStatus, at: Timestamp },
    /// The tab closed or the agent's process exited (not a herdr restart, not the machine sleeping).
    Closed { session_id: String, at: Timestamp },
}

/// What the node knows about a session from herdr, passed with each signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    pub session_id: String,
    pub agent: AgentKind,
    /// The herdr tab label.
    pub tab: Option<String>,
    pub cwd: Option<String>,
    /// The transcript file, when the node has resolved it. The module discovers it itself otherwise.
    pub transcript: Option<String>,
}

/// The module's settings, from `summaries:` in node.yaml. Every field has the plan's default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummariesConfig {
    pub enabled: bool,
    /// How long a session must stay idle before it's summarised.
    pub idle_seconds: u64,
    /// At most one summary per session in this window; an idle inside it is scheduled for when it ends.
    pub min_gap_seconds: u64,
    /// Summary calls at once on this machine.
    pub max_concurrent: u32,
    /// Input cap per call, in characters.
    pub input_cap_chars: u32,
    /// Old history is chunked by day for this many days, by ISO week before that.
    pub daily_chunk_days: u32,
    /// Run a sweep when the node starts and once a day.
    pub sweep: bool,
    /// The model for summary and catch-up calls.
    pub model: String,
    /// The person's name the prompts use ("since Sam's last message"). Empty means "the user".
    pub user_name: String,
}

impl Default for SummariesConfig {
    fn default() -> Self {
        SummariesConfig {
            enabled: true,
            idle_seconds: 60,
            min_gap_seconds: 30 * 60,
            max_concurrent: 2,
            input_cap_chars: 48_000,
            daily_chunk_days: 30,
            sweep: true,
            model: "sonnet".into(),
            user_name: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_serialises_with_kind_and_key() {
        let p = SummaryPeriod::Hour("2026-11-23T14".into());
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"kind":"hour","key":"2026-11-23T14"}"#);
    }

    #[test]
    fn config_fills_defaults() {
        let c: SummariesConfig = serde_json::from_str(r#"{"idle_seconds": 90}"#).unwrap();
        assert_eq!(c.idle_seconds, 90);
        assert_eq!(c.min_gap_seconds, 1800);
        assert_eq!(c.max_concurrent, 2);
    }
}
