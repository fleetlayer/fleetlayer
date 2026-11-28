-- Handoffs: one session's work moved to a new session (often on another node). The note is what the old session said
-- the new one needs; `from` and `to` are `<tab>@<node>`, with the session IDs when known, so the fleet shows the link.
CREATE TABLE handoffs (
    tenant TEXT NOT NULL,
    id TEXT NOT NULL,
    from_address TEXT NOT NULL,
    from_session_id TEXT,
    to_address TEXT NOT NULL,
    to_session_id TEXT,
    note TEXT,
    by_address TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, id)
);
CREATE INDEX handoffs_created ON handoffs (tenant, created_at);
