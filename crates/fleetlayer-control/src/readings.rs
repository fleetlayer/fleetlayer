//! Readings (`docs/protocol.md` section 9): numbers nodes publish, such as account usage and token health. The control
//! plane keeps the newest per kind, key and source node, and passes each new one to every linked node subscribed to
//! its kind that may read the source node.

use serde_json::{Value, json};
use sqlx::Row;

use crate::App;
use crate::db::Db;

/// Store readings from `node` and pass them on. A reading is `{kind, key, at, data}`; its `node` is set here.
pub async fn publish(app: &App, tenant: &str, node: &str, readings: Vec<Value>) -> anyhow::Result<()> {
    let mut stored = vec![];
    for mut r in readings {
        let (Some(kind), Some(key)) = (r["kind"].as_str().map(str::to_string), r["key"].as_str().map(str::to_string))
        else {
            continue;
        };
        let at = r["at"].as_str().and_then(|s| s.parse::<jiff::Timestamp>().ok()).unwrap_or_else(jiff::Timestamp::now);
        r["node"] = json!(node);
        sqlx::query(
            "INSERT INTO readings (tenant, kind, key, node, at, data) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (tenant, kind, key, node) DO UPDATE SET at = excluded.at, data = excluded.data
             WHERE excluded.at >= readings.at",
        )
        .bind(tenant)
        .bind(&kind)
        .bind(&key)
        .bind(node)
        .bind(at.as_millisecond())
        .bind(r["data"].to_string())
        .execute(&app.db)
        .await?;
        stored.push(r);
    }
    if stored.is_empty() {
        return Ok(());
    }
    // Pass them on to subscribed nodes that may read this node.
    let subs: Vec<((String, String), Vec<String>)> =
        app.links.reading_kinds.lock().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for ((t, sub), kinds) in subs {
        if t != tenant {
            continue;
        }
        if sub != node && !crate::authz::may_read(app, tenant, &format!("-@{sub}"), &format!("-@{node}")).await {
            continue;
        }
        let mine: Vec<&Value> =
            stored.iter().filter(|r| kinds.iter().any(|k| r["kind"].as_str() == Some(k.as_str()))).collect();
        if mine.is_empty() {
            continue;
        }
        if let Some(peer) = app.links.peer(tenant, &sub).await {
            peer.notify(fleetlayer_protocol::link::method::READING_UPDATE, &json!({"readings": mine}));
        }
    }
    Ok(())
}

/// The newest reading of each key and source node, for the given kinds.
pub async fn latest(db: &Db, tenant: &str, kinds: &[String]) -> anyhow::Result<Vec<Value>> {
    let mut out = vec![];
    for kind in kinds {
        let rows =
            sqlx::query("SELECT key, node, at, data FROM readings WHERE tenant = ? AND kind = ? ORDER BY key, node")
                .bind(tenant)
                .bind(kind)
                .fetch_all(db)
                .await?;
        for r in rows {
            let at = jiff::Timestamp::from_millisecond(r.get::<i64, _>(2)).map(|t| t.to_string()).unwrap_or_default();
            let data: String = r.get(3);
            out.push(json!({"kind": kind, "key": r.get::<String, _>(0), "node": r.get::<String, _>(1), "at": at,
                "data": serde_json::from_str::<Value>(&data).unwrap_or(Value::Null)}));
        }
    }
    Ok(out)
}

/// Every kind of reading there is, for `GET /v1/readings` without `?kind=`.
pub async fn kinds(db: &Db, tenant: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT DISTINCT kind FROM readings WHERE tenant = ? ORDER BY kind")
        .bind(tenant)
        .fetch_all(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_newest_reading_per_key_and_node_is_kept() {
        let db = crate::db::open_memory().await.unwrap();
        let put = |node: &'static str, at: &'static str, pct: i64| {
            let db = db.clone();
            async move {
                sqlx::query(
                    "INSERT INTO readings (tenant, kind, key, node, at, data) VALUES ('default', 'claude.usage', 'u1', ?, ?, ?)
                     ON CONFLICT (tenant, kind, key, node) DO UPDATE SET at = excluded.at, data = excluded.data
                     WHERE excluded.at >= readings.at",
                )
                .bind(node)
                .bind(at.parse::<jiff::Timestamp>().unwrap().as_millisecond())
                .bind(json!({"sevenDay": pct}).to_string())
                .execute(&db)
                .await
                .unwrap();
            }
        };
        put("hub", "2026-11-26T01:00:00Z", 40).await;
        put("hub", "2026-11-26T02:00:00Z", 50).await;
        put("hub", "2026-11-26T01:30:00Z", 45).await; // older: ignored
        put("laptop", "2026-11-26T01:00:00Z", 41).await;
        let l = latest(&db, "default", &["claude.usage".into()]).await.unwrap();
        assert_eq!(l.len(), 2);
        let hub = l.iter().find(|r| r["node"] == "hub").unwrap();
        assert_eq!(hub["data"]["sevenDay"], 50);
    }
}
