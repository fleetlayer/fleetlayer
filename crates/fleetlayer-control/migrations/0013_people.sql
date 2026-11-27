-- People, tenants, teams and memberships (docs/sso.md). Nothing here is a credential: passkeys, upstream accounts,
-- emails and sessions stay in the sign-in service.
--
-- A tenant's `id` never changes: it's the `tenant` column of every other table and part of every audit entry's
-- hash. Its `slug` and `name` are what people see and can be changed.
CREATE TABLE tenants (
    id TEXT NOT NULL PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
-- Everything so far lives in the tenant 'default'.
INSERT INTO tenants (id, slug, name, created_at) VALUES ('default', 'default', 'Default', 0);

-- People are global: one account each, the same `id` (the sign-in's `sub`) in every tenant.
CREATE TABLE people (
    id TEXT NOT NULL PRIMARY KEY,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- A person's membership of a tenant. A removed member's row stays (removed_at set), so its handle isn't given to
-- someone else while rules may still name it.
CREATE TABLE members (
    tenant TEXT NOT NULL REFERENCES tenants (id),
    person TEXT NOT NULL REFERENCES people (id),
    handle TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    added_at INTEGER NOT NULL,
    removed_at INTEGER,
    PRIMARY KEY (tenant, person),
    UNIQUE (tenant, handle)
);

CREATE TABLE teams (
    tenant TEXT NOT NULL REFERENCES tenants (id),
    name TEXT NOT NULL,
    parent TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
);

-- Every tenant has the team everyone is in (simple views don't show it until there's a second team).
INSERT INTO teams (tenant, name, parent, created_at) VALUES ('default', 'everyone', NULL, 0);

CREATE TABLE team_members (
    tenant TEXT NOT NULL,
    team TEXT NOT NULL,
    person TEXT NOT NULL REFERENCES people (id),
    role TEXT NOT NULL CHECK (role IN ('maintainer', 'member')),
    PRIMARY KEY (tenant, team, person),
    FOREIGN KEY (tenant, team) REFERENCES teams (tenant, name) ON DELETE CASCADE
);

-- Agents a tenant admits: one (`finances@laptop`), a node's (`*@laptop`), or a pattern.
CREATE TABLE member_agents (
    tenant TEXT NOT NULL REFERENCES tenants (id),
    pattern TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, pattern)
);

-- Invites by link: only the code's SHA-256 is kept.
CREATE TABLE invites (
    tenant TEXT NOT NULL REFERENCES tenants (id),
    code_hash TEXT NOT NULL PRIMARY KEY,
    handle TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    -- JSON list of team names.
    teams TEXT NOT NULL DEFAULT '[]',
    -- An existing person (recovery, or someone who already has an account); NULL for anyone.
    person TEXT REFERENCES people (id),
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    used_by TEXT,
    revoked_at INTEGER
);

-- A node's owner: a person (`p_…`, their device), a team (`team:<tenant id>/<name>`) or a tenant (`tenant:<id>`).
-- A team's or tenant's node stays with it when the person who enrolled it leaves.
ALTER TABLE nodes ADD COLUMN owner TEXT;
ALTER TABLE nodes ADD COLUMN enrolled_by TEXT;

-- Every existing node's agents stay members of 'default', so nothing that works today stops.
INSERT INTO member_agents (tenant, pattern, added_by, added_at)
    SELECT 'default', '*@' || name, 'migration', 0 FROM nodes WHERE tenant = 'default';

-- Audit entries written by commands (`fleetlayer-control member …`) for the running control plane to append to the
-- chain with its key: a command never holds the audit key, and never races the server for the next position.
CREATE TABLE audit_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    at INTEGER NOT NULL,
    decision TEXT NOT NULL,
    subject TEXT NOT NULL,
    verb TEXT NOT NULL,
    target TEXT NOT NULL,
    reason TEXT NOT NULL
);
