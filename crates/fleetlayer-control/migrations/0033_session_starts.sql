-- Sessions started or resumed through the control plane, for the policy's start limits (`starts_per_hour`,
-- `starts_per_day`): kept a day, so a limit holds across restarts.
CREATE TABLE session_starts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    who TEXT NOT NULL,
    target TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX session_starts_who ON session_starts (who, at);
