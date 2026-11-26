-- The access-control policy file, version 2 (docs/access-control.md), as applied with `acl apply`. One per tenant.
CREATE TABLE policy_files (
    tenant TEXT NOT NULL PRIMARY KEY,
    text TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);
