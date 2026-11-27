-- Windows of roles with `activation: window` (the admin window, `docs/access-control.md`): opened when the person
-- approves an `activate` request (an approval with verb 'activate' and target 'window:<role>', its id), closed at the
-- end or early. `pending_nodes` lists the role's nodes that haven't confirmed their clean-up (sessions closed, secrets
-- removed, hook run), as a JSON array; `problem` says why the last confirmation failed.
CREATE TABLE role_windows (
    tenant TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL PRIMARY KEY,
    role TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    opened_at INTEGER NOT NULL,
    ends_at INTEGER NOT NULL,
    closed_at INTEGER,
    closed_by TEXT,
    pending_nodes TEXT NOT NULL DEFAULT '[]',
    problem TEXT
);
CREATE INDEX role_windows_open ON role_windows (tenant, role, closed_at);
