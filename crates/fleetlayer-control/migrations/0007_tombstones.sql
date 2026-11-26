-- Deletions, for programs following the summaries feed: each appears in the feed in order with the records.
-- A NULL period means every record of that session from that node.
CREATE TABLE summary_tombstones (
    tenant TEXT NOT NULL DEFAULT 'default',
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node TEXT NOT NULL,
    session_id TEXT NOT NULL,
    period_kind TEXT,
    period_key TEXT,
    written_at INTEGER NOT NULL
);
CREATE INDEX summary_tombstones_feed ON summary_tombstones (tenant, written_at, id);
