-- Watches: "tell me when this happens", once. A watch matches like a subscription (a channel, maybe one subject) and
-- optionally only some kinds of event; it fires once and is removed, or ends at its deadline with a notice.
CREATE TABLE watches (
    tenant TEXT NOT NULL,
    id TEXT PRIMARY KEY,
    channel TEXT NOT NULL,
    subject TEXT,
    -- JSON array of event kinds; empty matches every kind.
    kinds TEXT NOT NULL DEFAULT '[]',
    target TEXT NOT NULL,
    note TEXT,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX watches_channel ON watches (tenant, channel);
CREATE INDEX watches_expiry ON watches (expires_at);
