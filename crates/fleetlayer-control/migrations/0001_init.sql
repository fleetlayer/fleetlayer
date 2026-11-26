-- Every table carries a tenant, so the hosted control plane runs the same schema as a self-hosted one (which uses
-- the tenant 'default'). Nothing here is a credential of the user's: node tokens and webhook secrets are stored only
-- as SHA-256 hashes, and they're ours, not the user's.

CREATE TABLE nodes (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    machine TEXT,
    user_name TEXT,
    version TEXT,
    public_key TEXT,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER,
    PRIMARY KEY (tenant, name)
);

-- The last session list each node pushed, kept so the fleet view shows offline nodes' sessions as last seen.
CREATE TABLE sessions (
    tenant TEXT NOT NULL DEFAULT 'default',
    node TEXT NOT NULL,
    session_id TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, node, session_id)
);

CREATE TABLE channels (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    -- Where an event goes when no subscription matches: '<tab>@<node>'.
    fallback TEXT,
    PRIMARY KEY (tenant, name)
);

CREATE TABLE subscriptions (
    tenant TEXT NOT NULL DEFAULT 'default',
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel TEXT NOT NULL,
    -- NULL matches every event on the channel.
    subject TEXT,
    target TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (tenant, channel, subject, target)
);

CREATE TABLE endpoints (
    tenant TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL PRIMARY KEY,
    secret_hash TEXT NOT NULL,
    label TEXT NOT NULL,
    channel TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    event_count INTEGER NOT NULL DEFAULT 0,
    last_event_at INTEGER
);

CREATE TABLE events (
    tenant TEXT NOT NULL DEFAULT 'default',
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel TEXT NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT,
    summary TEXT NOT NULL,
    body TEXT NOT NULL,
    source TEXT NOT NULL,
    dedupe_key TEXT,
    at INTEGER NOT NULL,
    -- JSON: [{target, ok, error?}]
    deliveries TEXT NOT NULL DEFAULT '[]'
);
CREATE UNIQUE INDEX events_dedupe ON events (tenant, channel, dedupe_key) WHERE dedupe_key IS NOT NULL;
