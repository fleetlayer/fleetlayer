-- Access control: ordered rules, first match wins, then the tenant's default. Patterns are '<tab>@<node>' with '*'
-- matching any run of characters ('*@agents-triage', 'sre@*'). A sender with no session (a shell, a plugin) is
-- '-@<node>'.
CREATE TABLE acl_rules (
    tenant TEXT NOT NULL DEFAULT 'default',
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    position INTEGER NOT NULL,
    from_pattern TEXT NOT NULL,
    to_pattern TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('allow', 'deny')),
    created_at INTEGER NOT NULL
);

CREATE TABLE tenant_settings (
    tenant TEXT NOT NULL PRIMARY KEY,
    acl_default TEXT NOT NULL DEFAULT 'allow' CHECK (acl_default IN ('allow', 'deny'))
);
