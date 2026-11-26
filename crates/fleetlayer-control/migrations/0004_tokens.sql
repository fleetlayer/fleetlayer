-- API tokens for programs reading the control plane (a dashboard, Quietdeck's deck). Stored as SHA-256 hashes.
-- Scopes: 'read' (every GET, and asking a node for a catch-up) and 'admin' (everything).
CREATE TABLE api_tokens (
    tenant TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'admin')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
);
