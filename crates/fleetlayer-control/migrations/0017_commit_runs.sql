-- GitHub's signing secret for an endpoint (X-Hub-Signature-256), kept in clear because verifying an HMAC needs it.
-- It's the receiver's own verifier, not a credential for anything else. NULL: no signature check.
ALTER TABLE endpoints ADD COLUMN github_secret TEXT;
-- 1: deliver every action of a workflow run (requested, in_progress); 0: only completed ones.
ALTER TABLE endpoints ADD COLUMN all_actions INTEGER NOT NULL DEFAULT 0;

-- The workflow runs seen for each commit, so a subscription to a commit ends once every run of it has completed, and
-- one made after a run finished still gets its result. Kept for a day.
CREATE TABLE commit_runs (
    tenant TEXT NOT NULL DEFAULT 'default',
    channel TEXT NOT NULL,
    repo TEXT NOT NULL,
    sha TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    workflow TEXT NOT NULL,
    -- A re-run is the same run with the next attempt: it starts pending again.
    attempt INTEGER NOT NULL,
    -- GitHub's action: requested, in_progress or completed.
    status TEXT NOT NULL,
    -- The stored event of the completed run, delivered to a subscription that comes after it.
    event_id INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, channel, repo, run_id)
);
CREATE INDEX commit_runs_sha ON commit_runs (tenant, channel, repo, sha);
