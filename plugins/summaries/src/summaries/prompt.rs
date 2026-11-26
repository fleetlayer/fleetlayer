//! The prompts and JSON schemas. The summary prompt and schema are claude-summarizer's, word for word
//! (`summarise.py`, exported into `prompts/` by Python so nothing was retyped).

use super::transcript::Agent;

const SUMMARY_SYSTEM: &str = include_str!("prompts/summary_system.txt");
pub const SUMMARY_SCHEMA: &str = include_str!("prompts/summary_schema.json");
const CATCHUP_SYSTEM: &str = include_str!("prompts/catchup_system.txt");
pub const CATCHUP_SCHEMA: &str = include_str!("prompts/catchup_schema.json");

pub fn summary_system(user: &str) -> String {
    SUMMARY_SYSTEM.replace("{user}", user)
}

pub fn catchup_system(user: &str) -> String {
    CATCHUP_SYSTEM.replace("{user}", user)
}

/// What the prompt says about the session.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent: Option<Agent>,
    pub tab: Option<String>,
    pub dir: Option<String>,
}

fn head(s: &SessionInfo) -> String {
    let agent = match s.agent {
        Some(Agent::Codex) => "codex",
        _ => "claude",
    };
    format!(
        "Session: tab \"{}\", agent {agent}, directory {}, session ID {}.",
        s.tab.as_deref().filter(|t| !t.is_empty()).unwrap_or("(no tab)"),
        s.dir.as_deref().filter(|d| !d.is_empty()).unwrap_or("?"),
        s.session_id
    )
}

/// `current` is the latest page and the name it was written under (claude-summarizer used the update's hour, such as
/// `2026-11-23T14`).
pub fn build_prompt(s: &SessionInfo, current: Option<(&str, &str)>, activity: &str, span: &str) -> String {
    let page = match current {
        Some((page, written)) if !page.is_empty() => {
            format!("Current page (written {written}):\n\n{page}")
        }
        _ => "Current page: none yet (first summary of this session).".to_owned(),
    };
    format!("{}\n\n{page}\n\n---\nNew activity ({span}):\n\n{activity}\n", head(s))
}

pub fn build_catchup_prompt(
    s: &SessionInfo,
    page: Option<&str>,
    last_message: &str,
    last_at: &str,
    activity: &str,
) -> String {
    let page = match page {
        Some(p) if !p.is_empty() => format!("Current page:\n\n{p}"),
        _ => "Current page: none yet.".to_owned(),
    };
    let activity = if activity.is_empty() { "(nothing yet)" } else { activity };
    format!(
        "{}\n\n{page}\n\n---\nTheir last message ({last_at}):\n\n{last_message}\n\n---\nSince then:\n\n{activity}\n",
        head(s)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_match_claude_summarizer() {
        let s = SessionInfo {
            session_id: "abc".into(),
            agent: Some(Agent::Claude),
            tab: Some("ci".into()),
            dir: Some("/x".into()),
        };
        assert_eq!(
            build_prompt(&s, Some(("PAGE", "2026-11-23T14")), "ACT", "SPAN"),
            "Session: tab \"ci\", agent claude, directory /x, session ID abc.\n\nCurrent page (written 2026-11-23T14):\n\nPAGE\n\n---\nNew activity (SPAN):\n\nACT\n"
        );
        let s = SessionInfo { session_id: "abc".into(), agent: Some(Agent::Codex), ..Default::default() };
        assert_eq!(
            build_prompt(&s, None, "ACT", "SPAN"),
            "Session: tab \"(no tab)\", agent codex, directory ?, session ID abc.\n\nCurrent page: none yet (first summary of this session).\n\n---\nNew activity (SPAN):\n\nACT\n"
        );
        let sys = summary_system("Sam");
        assert!(sys.starts_with("You keep the running record of one of Sam's AI coding agent sessions."));
        assert!(!sys.contains("{user}"));
        assert!(sys.ends_with("refer to records by their IDs instead.\n"));
        let schema: serde_json::Value = serde_json::from_str(SUMMARY_SCHEMA).unwrap();
        assert_eq!(schema["properties"]["status"]["enum"], serde_json::json!(["update", "no_change"]));
        serde_json::from_str::<serde_json::Value>(CATCHUP_SCHEMA).unwrap();
    }
}
