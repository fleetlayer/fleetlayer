-- When each person last used their passkey at the sign-in service (`POST /v1/signin/passkey`, over the sign-in
-- token): a change to a tenant's people made through a delegate token (the dashboard) needs a recent one, so a session
-- left open can't grant or take away rights.
CREATE TABLE passkey_uses (
    person TEXT NOT NULL PRIMARY KEY,
    at INTEGER NOT NULL
);
