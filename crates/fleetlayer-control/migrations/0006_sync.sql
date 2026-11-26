-- Sealed bundles that couldn't reach their target (it was offline). Only who owes whom what: the ciphertext is never
-- stored. When the target links again, the source is asked to seal a fresh copy.
CREATE TABLE sync_pending (
    tenant TEXT NOT NULL DEFAULT 'default',
    bundle TEXT NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    version INTEGER NOT NULL,
    since INTEGER NOT NULL,
    PRIMARY KEY (tenant, bundle, target)
);
