//! The one-off import of claude-summarizer's history: every session in its `index.json`, every Markdown update in
//! the session's folder (one timeline point each), and its byte offsets, so the node carries on where the hourly pass
//! stopped instead of summarising six months again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;

use super::state::{FileState, State};
use super::transcript::Agent;
use fleetlayer_protocol::summary::{RecordOrigin, SummaryPeriod, SummaryRecord};

#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    /// Oldest first per session.
    pub records: Vec<SummaryRecord>,
    pub sessions: usize,
    pub offsets: usize,
    /// Files that couldn't be read or parsed, with why.
    pub problems: Vec<String>,
}

static UPDATE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d\d-\d\dT\d\d\.md$").unwrap());
static SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^## (Sentence|Paragraph|Page)[ \t]*$").unwrap());

/// claude-summarizer wrote minutes-only times (`2026-10-23T11:18+13:00`).
fn parse_time(s: &str) -> Option<Timestamp> {
    s.parse::<Timestamp>().ok().or_else(|| {
        // Insert ":00" seconds after HH:MM.
        let (date_time, rest) = s.split_at(s.len().min(16));
        format!("{date_time}:00{rest}").parse().ok()
    })
}

/// Front matter (one JSON value per key) and the three sections.
pub fn parse_update(text: &str) -> (BTreeMap<String, Value>, BTreeMap<String, String>) {
    let mut front = BTreeMap::new();
    let mut body = text;
    if let Some((head, tail)) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---\n")) {
        for line in head.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                front.insert(k.to_owned(), serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_owned())));
            }
        }
        body = tail;
    }
    let mut tiers = BTreeMap::new();
    let heads: Vec<_> = SECTION.captures_iter(body).map(|c| (c.get(0).unwrap(), c[1].to_lowercase())).collect();
    for (i, (m, name)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map_or(body.len(), |(n, _)| n.start());
        tiers.insert(name.clone(), body[m.end()..end].trim().to_owned());
    }
    (front, tiers)
}

fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Read claude-summarizer's vault and state into records, and seed `state` with its offsets and each
/// session's latest page. Offsets already in `state` are kept (the node may have read further).
pub fn import(vault: &Path, old_state: Option<&Path>, state: &mut State) -> std::io::Result<Imported> {
    let index: Value = serde_json::from_slice(&std::fs::read(vault.join("index.json"))?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    import_index(vault, &index, old_state, state)
}

/// Where claude-summarizer kept its summaries: `vault_dir` in its config, or its default.
pub fn claude_summarizer_vault(home: &Path) -> PathBuf {
    let config = home.join(".config/claude-summarizer/config.json");
    let configured = std::fs::read(&config)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("vault_dir").and_then(Value::as_str).map(str::to_owned));
    match configured {
        Some(d) if d.starts_with("~/") => home.join(&d[2..]),
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home.join(".local/share/claude-summarizer"),
    }
}

/// At start-up on a machine that ran claude-summarizer: adopt its byte offsets and each session's latest page, so
/// nothing it already summarised is summarised again, whether or not anyone runs the import. Sends nothing to the
/// control plane: the import does that. Returns how many offsets it adopted, or None when there's nothing to adopt.
pub fn seed_from_claude_summarizer(home: &Path, state: &mut State) -> Option<usize> {
    let old_state = home.join(".local/state/claude-summarizer/state.json");
    if !old_state.exists() || state.seeded_at.is_some() || state.imported_at.is_some() {
        return None;
    }
    let vault = claude_summarizer_vault(home);
    let index = std::fs::read(vault.join("index.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| serde_json::json!({"sessions": {}}));
    match import_index(&vault, &index, Some(&old_state), state) {
        Ok(got) => {
            state.imported_at = None;
            state.seeded_at = Some(Timestamp::now());
            Some(got.offsets)
        }
        Err(e) => {
            tracing::warn!("summaries: can't adopt claude-summarizer's offsets from {}: {e}", old_state.display());
            None
        }
    }
}

fn import_index(vault: &Path, index: &Value, old_state: Option<&Path>, state: &mut State) -> std::io::Result<Imported> {
    let mut out = Imported::default();
    let empty = serde_json::Map::new();
    let sessions = index.get("sessions").and_then(Value::as_object).unwrap_or(&empty);
    for (sid, entry) in sessions {
        let agent =
            if entry.get("agent").and_then(Value::as_str) == Some("codex") { Agent::Codex } else { Agent::Claude };
        let tab = s(entry.get("tab")).or_else(|| s(entry.get("former_tab")));
        let dir = s(entry.get("dir"));
        let Some(folder) = s(entry.get("folder")) else {
            continue;
        };
        let mut names: Vec<String> = match std::fs::read_dir(vault.join(&folder)) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| UPDATE_NAME.is_match(n))
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        let mut latest: Option<SummaryRecord> = None;
        for name in names {
            let path = vault.join(&folder).join(&name);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    out.problems.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let (front, tiers) = parse_update(&text);
            let tier = |k: &str| tiers.get(k).cloned().unwrap_or_default();
            let time = |k: &str| s(front.get(k)).and_then(|v| parse_time(&v));
            let (Some(from), Some(to)) = (time("activity_from"), time("activity_to")) else {
                out.problems.push(format!("{}: no activity times", path.display()));
                continue;
            };
            if tier("sentence").is_empty() || tier("page").is_empty() {
                out.problems.push(format!("{}: missing a tier", path.display()));
                continue;
            }
            let record = SummaryRecord {
                session_id: sid.clone(),
                agent,
                period: SummaryPeriod::Hour(name.trim_end_matches(".md").to_owned()),
                activity_from: from,
                activity_to: to,
                written_at: time("written").unwrap_or(to),
                sentence: tier("sentence"),
                paragraph: tier("paragraph"),
                page: tier("page"),
                tab: s(front.get("tab")).or_else(|| tab.clone()),
                dir: s(front.get("directory")).or_else(|| dir.clone()),
                origin: RecordOrigin::ClaudeSummarizer,
            };
            latest = Some(record.clone());
            out.records.push(record);
        }
        let last_activity = entry
            .get("last_activity")
            .and_then(Value::as_f64)
            .and_then(|t| Timestamp::from_millisecond((t * 1000.0) as i64).ok());
        let transcripts: Vec<PathBuf> = entry
            .get("transcripts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
        let st = state.sessions.entry(sid.clone()).or_default();
        if let Some(l) = latest.as_ref().filter(|_| st.page.is_empty()) {
            st.page = l.page.clone();
            st.page_hour = super::engine::period_key(&l.period).to_owned();
        }
        st.last_activity = match (st.last_activity, last_activity) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        for t in transcripts {
            if !st.transcripts.contains(&t) {
                st.transcripts.push(t);
            }
        }
        out.sessions += 1;
    }
    if let Some(old) = old_state.filter(|p| p.exists()) {
        let v: Value = serde_json::from_slice(&std::fs::read(old)?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        for (path, f) in v.get("files").and_then(Value::as_object).into_iter().flatten() {
            let path = PathBuf::from(path);
            if state.files.contains_key(&path) {
                continue;
            }
            let Some(offset) = f.get("offset").and_then(Value::as_u64) else {
                continue;
            };
            // A pending failure count restarts: the node retries that chunk three more times.
            state.files.insert(path, FileState { offset, skip: s(f.get("skip")), ..Default::default() });
            out.offsets += 1;
        }
    }
    state.imported_at = Some(Timestamp::now());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPDATE: &str = "---\nsession_id: \"s1\"\ntab: \"ci\"\nagent: \"claude\"\ndirectory: \"/code/x\"\nactivity_from: \"2026-10-23T11:18+13:00\"\nactivity_to: \"2026-10-23T23:21+13:00\"\nwritten: \"2026-11-21T18:35:57+13:00\"\n---\n## Sentence\n\nOne line.\n\n## Paragraph\n\nPara.\n\n## Page\n\n# Heading\n\n## Not a tier heading\n\nBody.\n";

    #[test]
    fn parses_an_update_file() {
        let (front, tiers) = parse_update(UPDATE);
        assert_eq!(front["tab"], Value::String("ci".into()));
        assert_eq!(tiers["sentence"], "One line.");
        assert_eq!(tiers["page"], "# Heading\n\n## Not a tier heading\n\nBody.");
        assert_eq!(parse_time("2026-10-23T11:18+13:00"), Some("2026-10-22T22:18:00Z".parse().unwrap()));
        assert_eq!(parse_time("2026-11-21T18:35:57+13:00"), Some("2026-11-21T05:35:57Z".parse().unwrap()));
    }

    #[test]
    fn imports_sessions_updates_and_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        std::fs::create_dir_all(vault.join("ci")).unwrap();
        std::fs::write(vault.join("ci/2026-10-23T23.md"), UPDATE).unwrap();
        std::fs::write(
            vault.join("ci/2026-10-24T09.md"),
            UPDATE.replace("One line.", "Later line.").replace("23:21", "09:40"),
        )
        .unwrap();
        std::fs::write(vault.join("ci/notes.md"), "not an update").unwrap();
        std::fs::write(vault.join("index.json"), r#"{"sessions": {
            "s1": {"agent": "claude", "folder": "ci", "tab": "ci", "dir": "/code/x", "last_activity": 1795000000.5, "transcripts": ["/t/s1.jsonl"]},
            "s2": {"agent": "codex", "folder": "gone", "former_tab": "old", "transcripts": []}}}"#).unwrap();
        let old = vault.join("state.json");
        std::fs::write(&old, r#"{"files": {"/t/s1.jsonl": {"offset": 500}, "/t/h.jsonl": {"offset": 9, "skip": "headless run (sdk-cli)"}, "/t/f.jsonl": {"offset": 7, "failed_at": 90, "failures": 2}, "/t/mine.jsonl": {"offset": 1}}}"#).unwrap();
        let mut state = State::default();
        state.files.insert("/t/mine.jsonl".into(), FileState { offset: 77, ..Default::default() });
        let got = import(vault, Some(&old), &mut state).unwrap();
        assert_eq!(got.sessions, 2);
        assert_eq!(got.records.len(), 2);
        assert_eq!(got.records[1].sentence, "Later line.");
        assert_eq!(got.records[1].period, SummaryPeriod::Hour("2026-10-24T09".into()));
        assert_eq!(got.records[0].origin, RecordOrigin::ClaudeSummarizer);
        assert_eq!(state.sessions["s1"].page_hour, "2026-10-24T09");
        assert_eq!(state.sessions["s1"].transcripts, vec![PathBuf::from("/t/s1.jsonl")]);
        assert_eq!(state.sessions["s1"].last_activity, Some(Timestamp::from_millisecond(1_795_000_000_500).unwrap()));
        assert_eq!(got.offsets, 3);
        assert_eq!(state.files[Path::new("/t/s1.jsonl")].offset, 500);
        assert_eq!(state.files[Path::new("/t/h.jsonl")].skip.as_deref(), Some("headless run (sdk-cli)"));
        assert_eq!(state.files[Path::new("/t/f.jsonl")], FileState { offset: 7, ..Default::default() });
        assert_eq!(state.files[Path::new("/t/mine.jsonl")].offset, 77, "the node's own offsets win");
        assert!(state.imported_at.is_some());
    }
}
