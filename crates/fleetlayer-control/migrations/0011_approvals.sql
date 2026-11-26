-- Requests waiting for the person, and approvals that hold for a while (policy `ask:` rules).
CREATE TABLE approvals (
    tenant TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL PRIMARY KEY,
    subject TEXT NOT NULL,
    verb TEXT NOT NULL,
    target TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'denied')),
    hold_ms INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    expires_at INTEGER
);
CREATE INDEX approvals_lookup ON approvals (tenant, subject, verb, target, status);
