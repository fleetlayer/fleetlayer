//! Summaries in the control plane: stored as the nodes publish them, searchable with SQLite's FTS5, and the latest
//! one-liner shown on each session in the fleet view.

use fleetlayer_protocol::summary::{SummaryEvent, SummaryPeriod, SummaryRecord};
use serde_json::{Value, json};
use sqlx::Row;

use crate::db::Db;

fn period(p: &SummaryPeriod) -> (&'static str, &str) {
    match p {
        SummaryPeriod::Hour(k) => ("hour", k),
        SummaryPeriod::Day(k) => ("day", k),
        SummaryPeriod::Week(k) => ("week", k),
    }
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

pub async fn store(db: &Db, tenant: &str, node: &str, event: &SummaryEvent) -> anyhow::Result<()> {
    match event {
        SummaryEvent::Summary(r) => store_record(db, tenant, node, r).await,
        SummaryEvent::Activity { session_id, last_activity } => {
            sqlx::query(
                "INSERT INTO session_activity (tenant, session_id, last_activity) VALUES (?, ?, ?)
                 ON CONFLICT (tenant, session_id) DO UPDATE SET last_activity = MAX(last_activity, excluded.last_activity)",
            )
            .bind(tenant)
            .bind(session_id)
            .bind(last_activity.as_millisecond())
            .execute(db)
            .await?;
            Ok(())
        }
        SummaryEvent::Catchup(_) => Ok(()),
    }
}

async fn store_record(db: &Db, tenant: &str, node: &str, r: &SummaryRecord) -> anyhow::Result<()> {
    let (kind, key) = period(&r.period);
    sqlx::query(
        "INSERT INTO summaries (tenant, node, session_id, period_kind, period_key, agent, activity_from, activity_to,
                                written_at, sentence, paragraph, page, tab, dir, origin)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (tenant, session_id, period_kind, period_key) DO UPDATE SET
             node = excluded.node, agent = excluded.agent, activity_from = excluded.activity_from,
             activity_to = excluded.activity_to, written_at = excluded.written_at, sentence = excluded.sentence,
             paragraph = excluded.paragraph, page = excluded.page, tab = excluded.tab, dir = excluded.dir,
             origin = excluded.origin",
    )
    .bind(tenant)
    .bind(node)
    .bind(&r.session_id)
    .bind(kind)
    .bind(key)
    .bind(enum_str(&r.agent))
    .bind(r.activity_from.as_millisecond())
    .bind(r.activity_to.as_millisecond())
    .bind(r.written_at.as_millisecond())
    .bind(&r.sentence)
    .bind(&r.paragraph)
    .bind(&r.page)
    .bind(&r.tab)
    .bind(&r.dir)
    .bind(enum_str(&r.origin))
    .execute(db)
    .await?;
    Ok(())
}

macro_rules! columns {
    () => {
        "s.node, s.session_id, s.period_kind, s.period_key, s.agent, s.activity_from, s.activity_to, \
         s.sentence, s.paragraph, s.page, s.tab, s.dir, s.written_at, s.origin, s.rowid"
    };
}

fn row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "node": r.get::<String, _>(0), "sessionId": r.get::<String, _>(1),
        "period": {"kind": r.get::<String, _>(2), "key": r.get::<String, _>(3)},
        "agent": r.get::<String, _>(4), "activityFrom": r.get::<i64, _>(5), "activityTo": r.get::<i64, _>(6),
        "sentence": r.get::<String, _>(7), "paragraph": r.get::<String, _>(8), "page": r.get::<String, _>(9),
        "tab": r.get::<Option<String>, _>(10), "dir": r.get::<Option<String>, _>(11),
        "writtenAt": r.get::<i64, _>(12), "origin": r.get::<String, _>(13),
        "cursor": format!("{}:{}", r.get::<i64, _>(12), r.get::<i64, _>(14)),
    })
}

/// Delete a session's records published by one node (a session summarised by the wrong node). Returns how many.
pub async fn delete(db: &Db, tenant: &str, node: &str, session_id: &str) -> anyhow::Result<u64> {
    let r = sqlx::query("DELETE FROM summaries WHERE tenant = ? AND node = ? AND session_id = ?")
        .bind(tenant)
        .bind(node)
        .bind(session_id)
        .execute(db)
        .await?;
    // A tombstone, so programs following the feed drop their copies too.
    sqlx::query("INSERT INTO summary_tombstones (tenant, node, session_id, written_at) VALUES (?, ?, ?, ?)")
        .bind(tenant)
        .bind(node)
        .bind(session_id)
        .bind(crate::db::now_ms())
        .execute(db)
        .await?;
    Ok(r.rows_affected())
}

/// Full-text search, best match first. `q` is plain words: each is matched as a prefix.
pub async fn search(db: &Db, tenant: &str, q: &str, limit: i64) -> anyhow::Result<Vec<Value>> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect::<String>())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    if terms.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM summaries_fts f JOIN summaries s ON s.rowid = f.rowid
         WHERE summaries_fts MATCH ? AND s.tenant = ? ORDER BY bm25(summaries_fts) LIMIT ?"
    ))
    .bind(terms.join(" "))
    .bind(tenant)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_json).collect())
}

/// A feed position: records sort before tombstones written in the same millisecond, then by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    written: i64,
    tomb: bool,
    id: i64,
}

impl Pos {
    /// `<writtenAt>:<id>` (a record), `<writtenAt>:t<id>` (a tombstone), or a bare `<writtenAt>` (inclusive: just
    /// before anything written then).
    fn parse(s: &str) -> anyhow::Result<Pos> {
        Ok(match s.split_once(':') {
            Some((w, rest)) => match rest.strip_prefix('t') {
                Some(id) => Pos { written: w.parse()?, tomb: true, id: id.parse()? },
                None => Pos { written: w.parse()?, tomb: false, id: rest.parse()? },
            },
            None => Pos { written: s.parse()?, tomb: false, id: -1 },
        })
    }
}

/// Every record and tombstone written after `after` (an item's `cursor`), oldest first: the feed a program follows
/// to keep its own copy. A tombstone is `{deleted: true, node, sessionId, period: {kind, key} | null, cursor}`; a null
/// period means every record of that session from that node. A bare `<writtenAt ms>` is inclusive. A replaced record
/// gets a new `writtenAt`, so it comes round again.
pub async fn feed(db: &Db, tenant: &str, after: &str, limit: i64) -> anyhow::Result<Vec<Value>> {
    let a = Pos::parse(after)?;
    // Records strictly after the cursor: later writtenAt, or the same writtenAt and (the cursor is a record with a
    // lower id).
    let rec_id_after = if a.tomb { i64::MAX } else { a.id };
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM summaries s WHERE s.tenant = ?1 AND (s.written_at > ?2 OR (s.written_at = ?2 AND s.rowid > ?3))
         ORDER BY s.written_at, s.rowid LIMIT ?4"
    ))
    .bind(tenant)
    .bind(a.written)
    .bind(rec_id_after)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let tomb_id_after = if a.tomb { a.id } else { -1 };
    let tombs = sqlx::query(
        "SELECT id, node, session_id, period_kind, period_key, written_at FROM summary_tombstones
         WHERE tenant = ?1 AND (written_at > ?2 OR (written_at = ?2 AND id > ?3)) ORDER BY written_at, id LIMIT ?4",
    )
    .bind(tenant)
    .bind(a.written)
    .bind(tomb_id_after)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut items: Vec<(Pos, Value)> =
        rows.iter().map(|r| (Pos { written: r.get(12), tomb: false, id: r.get(14) }, row_json(r))).collect();
    for t in &tombs {
        let (id, written): (i64, i64) = (t.get(0), t.get(5));
        let period = match (t.get::<Option<String>, _>(3), t.get::<Option<String>, _>(4)) {
            (Some(kind), Some(key)) => json!({"kind": kind, "key": key}),
            _ => Value::Null,
        };
        items.push((
            Pos { written, tomb: true, id },
            json!({"deleted": true, "node": t.get::<String, _>(1), "sessionId": t.get::<String, _>(2),
                   "period": period, "writtenAt": written, "cursor": format!("{written}:t{id}")}),
        ));
    }
    items.sort_by_key(|(p, _)| *p);
    items.truncate(limit.max(0) as usize);
    Ok(items.into_iter().map(|(_, v)| v).collect())
}

/// A session's timeline, oldest first: every record.
pub async fn timeline(db: &Db, tenant: &str, session_id: &str, limit: i64) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM summaries s WHERE s.tenant = ? AND s.session_id = ? ORDER BY s.activity_from, s.activity_to LIMIT ?"
    ))
    .bind(tenant)
    .bind(session_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_json).collect())
}

/// Sessions whose summaries mention `text` exactly (an ID such as `sc-1234`, a PR number, a Sentry short ID), with the
/// newest mentioning record of each, newest first.
pub async fn mentions(db: &Db, tenant: &str, text: &str, limit: i64) -> anyhow::Result<Vec<Value>> {
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    let pattern = format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM summaries s WHERE s.tenant = ?1
           AND (s.sentence LIKE ?2 ESCAPE '\\' OR s.paragraph LIKE ?2 ESCAPE '\\' OR s.page LIKE ?2 ESCAPE '\\')
           AND s.activity_to = (SELECT MAX(t.activity_to) FROM summaries t WHERE t.tenant = s.tenant
                                AND t.session_id = s.session_id
                                AND (t.sentence LIKE ?2 ESCAPE '\\' OR t.paragraph LIKE ?2 ESCAPE '\\' OR t.page LIKE ?2 ESCAPE '\\'))
         ORDER BY s.activity_to DESC LIMIT ?3"
    ))
    .bind(tenant)
    .bind(pattern)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_json).collect())
}

/// The newest record's three tiers and the last activity of every session that has one.
pub async fn latest(db: &Db, tenant: &str) -> anyhow::Result<std::collections::HashMap<String, Value>> {
    let rows = sqlx::query(
        "SELECT s.session_id, s.sentence, s.activity_to, a.last_activity, s.paragraph, s.page FROM summaries s
         LEFT JOIN session_activity a ON a.tenant = s.tenant AND a.session_id = s.session_id
         WHERE s.tenant = ? AND s.activity_to = (SELECT MAX(activity_to) FROM summaries t
                                                 WHERE t.tenant = s.tenant AND t.session_id = s.session_id)",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let id: String = r.get(0);
            let last = r.get::<Option<i64>, _>(3).unwrap_or(r.get(2));
            let (paragraph, page): (String, String) = (r.get(4), r.get(5));
            (id, json!({"sentence": r.get::<String, _>(1), "paragraph": paragraph, "page": page, "lastActivity": last}))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleetlayer_protocol::summary::{AgentKind, RecordOrigin};

    fn rec(id: &str, hour: &str, sentence: &str) -> SummaryEvent {
        let t: jiff::Timestamp = format!("2026-01-02T{hour}:00:00Z").parse().unwrap();
        SummaryEvent::Summary(SummaryRecord {
            session_id: id.into(),
            agent: AgentKind::Claude,
            period: SummaryPeriod::Hour(format!("2026-11-26T{hour}")),
            activity_from: t,
            activity_to: t,
            written_at: t,
            sentence: sentence.into(),
            paragraph: String::new(),
            page: "Details about the control plane.".into(),
            tab: Some("fleetlayer".into()),
            dir: None,
            origin: RecordOrigin::Node,
        })
    }

    #[tokio::test]
    async fn summaries_store_replace_search_and_give_the_latest() {
        let db = crate::db::open_memory().await.unwrap();
        let t = crate::db::DEFAULT_TENANT;
        store(&db, t, "agents", &rec("s1", "01", "Wrote the protocol draft.")).await.unwrap();
        store(&db, t, "agents", &rec("s1", "02", "Built the summaries plugin.")).await.unwrap();
        store(&db, t, "agents", &rec("s1", "02", "Built and tested the summaries plugin.")).await.unwrap();
        assert_eq!(timeline(&db, t, "s1", 10).await.unwrap().len(), 2, "the same hour replaces");
        let hits = search(&db, t, "summar", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["sentence"], "Built and tested the summaries plugin.");
        assert_eq!(search(&db, t, "control plane", 10).await.unwrap().len(), 2, "the page is searched too");
        assert!(search(&db, t, "\"; DROP", 10).await.unwrap().is_empty());
        assert_eq!(latest(&db, t).await.unwrap()["s1"]["sentence"], "Built and tested the summaries plugin.");
        assert_eq!(latest(&db, t).await.unwrap()["s1"]["page"], "Details about the control plane.");
        let tl = timeline(&db, t, "s1", 10).await.unwrap();
        assert_eq!(tl[0]["sentence"], "Wrote the protocol draft.", "oldest first");
        assert_eq!(tl[0]["origin"], "node");
        store(&db, t, "agents", &rec("s2", "03", "Fixed sc-1234 and sc-12345.")).await.unwrap();
        store(&db, t, "agents", &rec("s2", "04", "Shipped sc-1234.")).await.unwrap();
        let m = mentions(&db, t, "sc-1234", 10).await.unwrap();
        assert_eq!(m.len(), 1, "one per session");
        assert_eq!(m[0]["sentence"], "Shipped sc-1234.", "the newest mention");
        assert!(mentions(&db, t, "sc-9", 10).await.unwrap().is_empty());
        assert!(mentions(&db, t, "100%", 10).await.unwrap().is_empty(), "% is literal");
        // The feed pages by cursor, ties on writtenAt included.
        let all = feed(&db, t, "0", 100).await.unwrap();
        assert_eq!(all.len(), 4);
        let first_two = feed(&db, t, "0", 2).await.unwrap();
        let rest = feed(&db, t, first_two[1]["cursor"].as_str().unwrap(), 100).await.unwrap();
        assert_eq!(first_two.len() + rest.len(), 4);
        assert_ne!(first_two[1]["cursor"], rest[0]["cursor"]);
        assert!(feed(&db, t, "bad", 10).await.is_err());
        // A deletion appears in the feed after the records, as a tombstone.
        let end = all[3]["cursor"].as_str().unwrap().to_string();
        assert_eq!(delete(&db, t, "agents", "s2").await.unwrap(), 2);
        let after = feed(&db, t, &end, 10).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["deleted"], true);
        assert_eq!(after[0]["sessionId"], "s2");
        assert!(after[0]["period"].is_null());
        assert!(feed(&db, t, after[0]["cursor"].as_str().unwrap(), 10).await.unwrap().is_empty(), "nothing after it");
        let last = all[3]["writtenAt"].as_i64().unwrap().to_string();
        assert!(!feed(&db, t, &last, 10).await.unwrap().is_empty(), "a bare time is inclusive");
    }
}
