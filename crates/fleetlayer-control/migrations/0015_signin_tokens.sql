-- The sign-in service's tokens (docs/sso.md, step 1c): separate from node and API tokens. One reads memberships, redeems
-- invites, records sign-ins and asks checks, for every tenant, and nothing else. Stored as SHA-256 hashes.
CREATE TABLE signin_tokens (
    name TEXT NOT NULL PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
