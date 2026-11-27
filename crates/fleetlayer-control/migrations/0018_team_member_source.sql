-- Who put someone in a team: NULL for an admin (or an invite), else the upstream provider whose groups map to it
-- (docs/sso.md, "Upstream providers"). A provider's memberships are replaced at each sync; an admin's are kept.
ALTER TABLE team_members ADD COLUMN source TEXT;
