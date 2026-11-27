-- Role windows, after review: the nodes each window concerns are recorded when it opens (and widened when it closes),
-- as a JSON array of {"home", "node", "member"}, so a policy change mid-window still tells the nodes it opened on, and
-- nodes that only watch the role (the one that adds and removes a sudo rule) must confirm the close like the role's
-- own. The roles each node watches are kept here, not only while it's linked. And who decided each approval.
ALTER TABLE role_windows ADD COLUMN nodes TEXT NOT NULL DEFAULT '[]';
CREATE TABLE role_watchers (
    tenant TEXT NOT NULL,
    node TEXT NOT NULL,
    role TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, node, role)
);
ALTER TABLE approvals ADD COLUMN decided_by TEXT;
-- Who ended an approval early (a window closed with `role deactivate`), kept even if the window wasn't recorded yet.
ALTER TABLE approvals ADD COLUMN ended_by TEXT;
