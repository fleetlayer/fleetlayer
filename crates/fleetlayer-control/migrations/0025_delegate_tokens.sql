-- API tokens may have the 'delegate' scope: a service people sign in to (the dashboard) that reads like a read token
-- and decides approvals and closes role windows only as the signed-in person it names (`Fleetlayer-Person`), one of the
-- tenant's admins. SQLite can't change a CHECK constraint, so the table is rebuilt.
CREATE TABLE api_tokens_new (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'admin', 'send', 'approve', 'delegate')),
    created_at INTEGER NOT NULL,
    owner TEXT,
    PRIMARY KEY (tenant, name)
);
INSERT INTO api_tokens_new (tenant, name, token_hash, scope, created_at, owner)
    SELECT tenant, name, token_hash, scope, created_at, owner FROM api_tokens;
DROP TABLE api_tokens;
ALTER TABLE api_tokens_new RENAME TO api_tokens;
