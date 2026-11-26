-- Readings: numbers nodes publish (usage, token health). The newest per kind, key and source node.
CREATE TABLE readings (
    tenant TEXT NOT NULL DEFAULT 'default',
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    node TEXT NOT NULL,
    at INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (tenant, kind, key, node)
);
