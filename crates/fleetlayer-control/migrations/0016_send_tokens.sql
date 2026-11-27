-- API tokens may also have the 'send' scope: a service (Quietdeck's deck) sends messages as service:<name>, and can't
-- read or administer anything. SQLite can't change a CHECK constraint, so the table is rebuilt.
CREATE TABLE api_tokens_new (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'admin', 'send')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
);
INSERT INTO api_tokens_new (tenant, name, token_hash, scope, created_at)
    SELECT tenant, name, token_hash, scope, created_at FROM api_tokens;
DROP TABLE api_tokens;
ALTER TABLE api_tokens_new RENAME TO api_tokens;
