-- The tamper-evident audit log: every access decision, chained. Each row's hash is an HMAC (keyed outside the
-- database) over its fields and the previous row's hash, so a changed, reordered or removed row breaks the chain.
CREATE TABLE audit_log (
    tenant TEXT NOT NULL DEFAULT 'default',
    seq INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    decision TEXT NOT NULL,
    subject TEXT NOT NULL,
    verb TEXT NOT NULL,
    target TEXT NOT NULL,
    reason TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 1,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL
);
-- Append-only: rows can't be changed or deleted through SQL (a person with the file can still edit it; the chain
-- shows it).
CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
