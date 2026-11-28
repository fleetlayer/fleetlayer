-- Every tab name each session has gone by, kept after it ends, so the policy still checks an ended session (read by its
-- ID, as a transcript search finds it) under its tab's rules. Forgotten a year after it was last seen.
CREATE TABLE session_names (
    tenant TEXT NOT NULL,
    node TEXT NOT NULL,
    session_id TEXT NOT NULL,
    tab TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, node, session_id, tab)
);
CREATE INDEX session_names_seen ON session_names (seen_at);
