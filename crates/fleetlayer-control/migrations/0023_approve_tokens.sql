-- API tokens may have the 'approve' scope: a deck or inbox that approves and refuses requests and closes role windows,
-- and reads approvals and windows, nothing else. Such a token has an owner (a person of its tenant), recorded as the
-- approver of what it decides. SQLite can't change a CHECK constraint, so the table is rebuilt.
CREATE TABLE api_tokens_new (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'admin', 'send', 'approve')),
    created_at INTEGER NOT NULL,
    owner TEXT,
    PRIMARY KEY (tenant, name)
);
INSERT INTO api_tokens_new (tenant, name, token_hash, scope, created_at)
    SELECT tenant, name, token_hash, scope, created_at FROM api_tokens;
DROP TABLE api_tokens;
ALTER TABLE api_tokens_new RENAME TO api_tokens;

-- A person passed a passkey check at the sign-in service for one approval (`POST /v1/signin/stepup`): an approve
-- token's approval of a role window needs one from its owner, used once, within a few minutes.
CREATE TABLE approval_stepups (
    tenant TEXT NOT NULL,
    approval_id TEXT NOT NULL,
    person TEXT NOT NULL,
    at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    signin TEXT NOT NULL
);
CREATE INDEX approval_stepups_lookup ON approval_stepups (tenant, approval_id);
